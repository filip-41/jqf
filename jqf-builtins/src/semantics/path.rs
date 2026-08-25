//! The path vocabulary and the value laws `getpath`/`setpath` read and write.
//!
//! One job: own the single authoritative definition of what a PATH is — an array of components, each a string key, a
//! number index, or a `{"start":…,"end":…}` slice object — plus the two owned-value laws that consume one:
//! reading the value a path addresses, and writing a value at a path (extending missing containers with the
//! null-extension rule).
//!
//! Every component is read as a raw [`Value`], exactly as the path law reads it, and every `(container kind, component
//! kind)` pair is dispatched here, because the error CLASS is a function of that pair and of nothing else: an object
//! indexed with an array is the index class, an array UPDATED at an array component is the update class, and an array
//! sliced with a malformed slice object is the slice-bound class. Deriving a typed component first and dispatching
//! second would collapse pairs the law keeps apart.
//!
//! [`PathStep`] is the complementary half: the vocabulary the path-mode interpreter accumulates as it navigates, and
//! which it renders back into the component form above when it emits a path.
//!
//! Negative space: it drives no cardinality, evaluates no subgraph, and knows nothing about deletion — the deletion
//! law is a set law over many paths at once and lives in [`super::pathset`].

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use jqf_data::{Array, Float, Number, Object, ObjectBuilder, Shared, Value, ValueKind};
use jqf_resource::ResourceContext;

use crate::error::EngineRunError;
use crate::error::message;
use crate::semantics::arith::{ceil_binary64, floor_binary64};
use crate::semantics::depth::{self, Guarded};
use crate::semantics::order;

/// Whether each path in a set is a strict prefix of a LATER path in the set.
///
/// The engine crate's assignment fold reads this once that crate lands. The in-place read ([`vacate_path`]) VACATES the
/// addressed slot — it leaves `null` in it — which is invisible as long as no LATER path descends through that
/// slot:
/// the deferred deletion removes the hole together with the path it sits in. But a path that a LATER path descends
/// through must be read WITHOUT vacating:
/// `_modify` never touches its accumulator until an update emits, so the descendant reads the INTACT value, and a
/// vacated hole would answer `null` where the value is the answer — `recurse |= (.+1)?` errored `Cannot index number
/// with number (0)` for exactly this reason.
///
/// The order-dependent "later" qualifier is the exact condition: an EARLIER path's extension ran before this path's
/// step, so it never walks the hole; only a later descendant does.
pub fn later_extensions(paths: &[Value], resources: &ResourceContext<'_>) -> Result<Vec<bool>, EngineRunError> {
    let mut seen: BTreeSet<PathKey<'_>> = BTreeSet::new();
    let mut out = Vec::new();
    out.try_reserve_exact(paths.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    // Walk BACKWARD so `seen` holds exactly the later paths. A path is a strict prefix of some later path iff the FIRST
    // seen path at or above it is a strict extension: every path with `key` as a prefix sorts below any path that
    // diverges from it (the divergence point compares greater), so the range scan stops at the first non-extension.
    //
    // Keys are borrowed from the caller's path arrays — the question is only whether ANY later path extends this one,
    // so a cloned component list cannot change the answer. A path that already has a later extension is not inserted:
    // any earlier path that would have used it as a witness is also a prefix of that same later leaf, which is already
    // in `seen`.
    for path in paths.iter().rev() {
        let key = PathKey {
            components: path_components(path, resources)?,
        };
        let has_extension = {
            let mut found = false;
            for candidate in seen.range(&key..) {
                if key.strict_prefix_of(*candidate) {
                    found = true;
                    break;
                }
                if candidate != &key {
                    break;
                }
            }
            found
        };
        out.push(has_extension);
        if !has_extension {
            seen.insert(key);
        }
    }
    out.reverse();
    Ok(out)
}

/// One path as an orderable component sequence, for [`later_extensions`].
///
/// ONE comparison law inside the key: the order is [`order::total_cmp`]'s lexicographic extension (a strict prefix
/// sorts before its extensions), and equality is that SAME comparator's `Equal` — never the observable operators'
/// law, whose `nan != nan` leaf would break the `Ord`⇔`Eq` contract a `BTreeSet` key must hold (the same reason an
/// ordered index declines NaN-carrying keys). Observable equality is the operators' answer, not a container's keying
/// law.
///
/// The components stay borrowed from the caller's path value: cloning them would allocate a second list whose identity
/// the comparison never reads.
#[derive(Clone, Copy)]
struct PathKey<'a> {
    components: &'a Array,
}

impl PathKey<'_> {
    /// Whether two components compare `Equal` under the key's one law.
    ///
    /// [`TooDeep`] answers `false` — the conservative read the ordering side mirrors with `Equal`.
    fn components_equal(a: &Value, b: &Value) -> bool {
        order::total_cmp(a, b).is_ok_and(|ordering| ordering == core::cmp::Ordering::Equal)
    }

    /// Whether `self` is a STRICT prefix of `other`: component-wise equal and strictly shorter.
    fn strict_prefix_of(self, other: Self) -> bool {
        if self.components.len() >= other.components.len() {
            return false;
        }
        self.components
            .iter()
            .zip(other.components)
            .all(|(a, b)| Self::components_equal(a, b))
    }
}

impl PartialEq for PathKey<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.components.len() == other.components.len()
            && self
                .components
                .iter()
                .zip(other.components)
                .all(|(a, b)| Self::components_equal(a, b))
    }
}
impl Eq for PathKey<'_> {}
impl PartialOrd for PathKey<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for PathKey<'_> {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        for (a, b) in self.components.iter().zip(other.components) {
            // A decline cannot fire here: the components are the fold's own path values, and the parse nesting cap
            // equals the comparison row's ceiling, so any component that can exist compares within it. The conservative
            // `Equal` mirrors [`Self::strict_prefix_of`]'s read of the same unreachable error.
            let ordering = order::total_cmp(a, b);
            // The fallback below exists for totality, never for execution; the assertion keeps the cannot-fire claim
            // honest.
            debug_assert!(
                ordering.is_ok(),
                "path-key component comparison exceeded the depth ceiling"
            );
            let ordering = ordering.unwrap_or(core::cmp::Ordering::Equal);
            if ordering != core::cmp::Ordering::Equal {
                return ordering;
            }
        }
        self.components.len().cmp(&other.components.len())
    }
}

/// The slice component's two member names, in the law's order.
pub const SLICE_START: &str = "start";
/// See [`SLICE_START`].
pub const SLICE_END: &str = "end";

const INDEX_MIN: f64 = -2_147_483_648.0;
/// See [`INDEX_MIN`].
const INDEX_MAX: f64 = 2_147_483_647.0;

/// The largest position a WRITE may address, which is a quarter of the range a READ may name.
///
/// Writes cap at a quarter of the read range instead of accepting anything — the cap exists because a write past the
/// end NULL-EXTENDS, so an unbounded index is an unbounded allocation request rather than a miss.
/// Reads keep the wider [`INDEX_MAX`] range: `.[999999999]` on a short array is an ordinary `null`, not an error.
const WRITE_INDEX_MAX: usize = (i32::MAX >> 2) as usize;

/// One accumulated component of a tracked path.
///
/// This is the interpreter's vocabulary, not the wire form: it is what a navigation step APPENDS to the register, and
/// [`step_value`] renders it back into the component value `path` finally emits.
#[derive(Debug, Clone)]
pub enum PathStep {
    /// An object member, by name, as SHARED text.
    ///
    /// The register is read far more often than it is written: a step at depth `d` is rendered once per path emitted
    /// from anywhere beneath it, so a key held BY VALUE is copied once per descendant leaf. Held shared, the push pays
    /// for the text once and every rendering is a refcount bump.
    ///
    /// The allocation must carry its OWN residency — one the push allocated ([`charged_key`]), or one a
    /// [`Value::String`] already paid for. An object entry's [`ObjectKey`](jqf_data::ObjectKey) is the single text
    /// allocation that carries none, because the entry table prices key bytes per ENTRY; a step sharing that allocation
    /// would still be holding it after the object dropped, against a charge the ledger already released.
    Key(Shared<str>),
    /// An array position, as authored (a negative index counts from the end).
    Index(i64),
    /// An array position addressed by a NaN number (`.[nan]`).
    ///
    /// A NaN index is a NUMBER component — `path(.[nan])` emits `[null]` only because NaN renders as null, and
    /// `getpath([nan])`/`setpath([nan]; …)`/ `delpaths([[nan]])` all see the NaN number itself, answering null, the
    /// `Cannot set array element at NaN index` refusal, and a no-op. Rendered as a NaN number ([`step_value`]), every
    /// consumer downstream of [`get_component`] inherits those laws from the existing NaN handling in the array/object
    /// component arms.
    IndexNaN,
    /// An array or string slice; an absent bound is `null` in the emitted form.
    ///
    /// The bounds are the AUTHORED values, not the resolved positions: the emitted form keeps `{"start":1.5,"end":3}`
    /// for `.[1.5:3]`, and a resolved `1` would be a different path.
    Slice {
        /// The lower bound, or `None` for an open one.
        start: Option<Value>,
        /// The upper bound, or `None` for an open one.
        end: Option<Value>,
    },
    /// The subsequence-search component (`.[[1,2]]`): the array key is emitted as the path component and navigates
    /// through the same `get_component` law the step applies.
    Subsequence(Value),
    /// The slice-OBJECT component (`.[{"start":a,"end":b}]`): the AUTHORED object is emitted VERBATIM — a missing
    /// `end` stays absent, an extra key stays present — and the navigation reads `start`/`end` out of it through the
    /// same `get_component` law. Distinct from the STATIC slice ([`PathStep::Slice`]), whose authored-bounds form
    /// renders `null` for an open bound.
    SliceObject(Value),
}

/// Renders one accumulated step as the component value the path emits.
///
/// A slice renders as the two-member object `{"start":…,"end":…}` with `null` for an open bound — the same
/// spelling `getpath`/`setpath` read back.
///
/// A key renders by RETAINING its text rather than copying it: the component is a second handle on the allocation the
/// step holds, which is the same charge-once discipline [`Value::try_clone`] follows.
pub fn step_value(step: &PathStep, _resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    match step {
        PathStep::Key(name) => Ok(Value::String(name.clone_shared())),
        PathStep::Index(index) => Ok(integer(*index)),
        PathStep::IndexNaN => Ok(Value::Number(Number::float(Float::new(f64::NAN)))),
        PathStep::Subsequence(needle) => Ok(try_clone(needle)),
        PathStep::SliceObject(bounds) => Ok(try_clone(bounds)),
        PathStep::Slice { start, end } => {
            let mut builder = ObjectBuilder::try_with_capacity(2).map_err(|_| EngineRunError::allocation_failure())?;
            for (name, bound) in [(SLICE_START, start), (SLICE_END, end)] {
                let bound = match bound {
                    Some(bound) => try_clone(bound),
                    None => Value::Null,
                };
                builder
                    .try_insert_last(clone_key(name)?, bound)
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
            builder
                .try_finish()
                .map(Value::Object)
                .map_err(|_| EngineRunError::allocation_failure())
        }
    }
}

/// Renders an accumulated register as the path array `path(f)` emits.
pub fn path_value(steps: &[PathStep], resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(steps.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for step in steps {
        values.push(step_value(step, resources)?);
    }
    Array::try_from_vec(values)
        .map(Value::Array)
        .map_err(|_| EngineRunError::allocation_failure())
}

/// Reads the value one path addresses, or `null` for an absent member.
///
/// `path` is the raw runtime value, not a pre-validated array: the "Path must be specified as an array" rejection is
/// this function's own.
pub fn get_path(root: &Value, path: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let components = path_components(path, resources)?;
    let mut current = try_clone(root);
    for component in components {
        current = get_component(&current, component, resources)?;
    }
    Ok(current)
}

/// Writes `new` at `path`, extending missing containers under the null rule.
///
/// The walk is two-pass and ITERATIVE — descend collecting each level's container, then rebuild from the leaf upwards
/// — so a long path array costs heap, never native stack.
///
/// `root` is taken by VALUE, and that is the whole cost story: the descent VACATES each addressed slot
/// ([`take_component`]) instead of reading a second handle out of it, so every container on the chain arrives at its
/// write uniquely owned and the write lands in place. A caller that still needs its root passes a `try_clone` of it and
/// pays exactly the copy-on-write detach it asked for; a caller that hands its only handle over — the assignment
/// fold's accumulator — turns an O(container) rebuild per write into O(1).
pub fn set_path(
    root: Value,
    path: &Value,
    new: Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let components = path_components(path, resources)?;
    let mut chain = Vec::new();
    chain
        .try_reserve_exact(components.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    let mut current = root;
    for component in components {
        let (child, slot) = take_component(&mut current, component, resources)?;
        chain.push((current, slot));
        current = child;
    }
    write_set_chain(chain, components, new, resources)
}

/// Rebuilds one [`set_path`] descent from the leaf upwards.
///
/// A level whose descent VACATED a slot rebuilds by POSITION: the container was not reshaped in between (its slot
/// merely holds the hole), so the write lands without a second key lookup. Every other level — a missing member to
/// vivify, a slice, a tagged container — takes the general component write.
fn write_set_chain(
    chain: Vec<(Value, Option<ComponentSlot>)>,
    components: &Array,
    new: Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let mut updated = new;
    for (component, (container, slot)) in components.iter().rev().zip(chain.into_iter().rev()) {
        updated = match slot {
            Some(slot) => {
                let mut container = container;
                refill_slot(&mut container, &slot, updated, resources)?;
                container
            }
            None => set_component(container, component, updated, resources)?,
        };
    }
    Ok(updated)
}

/// What [`take_path`] made of the root it was handed.
#[cfg(test)]
pub enum Vacated {
    /// The addressed value, VACATED, and the root with `null` left in its slot.
    Taken { value: Value, root: Value },
    /// The path does not address an already-existing slot at every component, so nothing was touched and the root comes
    /// back exactly as it went in.
    Declined(Value),
}

/// Ancestor containers a vacating descent recorded, each addressed slot holding `null`. The matching write refills
/// these slots instead of walking the path again.
pub struct VacatedChain {
    levels: Vec<(Value, ComponentSlot)>,
}

impl VacatedChain {
    /// Relink the holes so a caller that holds one `Value` sees a connected tree. The deepest container already holds
    /// the leaf's hole — writing `null` over it again is what the old rebuild loop did — so the relink starts one
    /// level up.
    #[cfg(test)]
    fn relink_holes(mut self, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
        let mut root = match self.levels.pop() {
            Some((container, _slot)) => container,
            None => Value::Null,
        };
        for (mut container, slot) in self.levels.into_iter().rev() {
            refill_slot(&mut container, &slot, root, resources)?;
            root = container;
        }
        Ok(root)
    }

    /// Write `new` into the vacated slots, leaf-first, by recorded position.
    ///
    /// The assignment fold keeps the chain and writes here instead of walking the path again. A caller that holds a
    /// single root relinks the holes into one `Value`; the tests pin this refill against [`set_path`].
    pub fn write(self, new: Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
        let mut updated = new;
        for (mut container, slot) in self.levels.into_iter().rev() {
            refill_slot(&mut container, &slot, updated, resources)?;
            updated = container;
        }
        Ok(updated)
    }
}

/// One vacating descent: either the addressed value plus the recorded chain, or the restored root when a level was not
/// an already-existing slot.
pub enum VacateResult {
    Taken { value: Value, chain: VacatedChain },
    Declined(Value),
}

/// Reads the value one path addresses by VACATING it — the read half of [`set_path`]'s cost story.
///
/// [`get_path`] answers a second handle on the addressed value, which is the right answer for a caller that keeps its
/// root. It is the wrong one for the assignment fold, whose whole purpose is to hand that value to an update and write
/// the result back: the accumulator still names the value, so the update's operand is SHARED, and a `+` that would have
/// grown its left operand in place detaches and rebuilds instead. Over a fold that is O(container) per step and O(n²)
/// over the run — `reduce .[] as $x ({"a":[]}; .a += $x)` was quadratic for exactly this reason, while the identical
/// `reduce .[] as $x ([]; . + $x)` spelling was already linear because a `Binary` hands its left operand over MOVED.
///
/// So this leaves `null` in the slot instead, and the value arrives at the update as the last handle on its allocation.
/// The descent records each vacated container; the matching write refills those recorded slots and does not walk the
/// path again. [`take_path_chain`] is the form that keeps the chain.
/// Callers that only hold the relinked holed root still walk through [`set_path`].
///
/// It DECLINES — having touched nothing — unless every component addresses a slot that ALREADY EXISTS: an untagged
/// object with the key present, or an untagged array with the index in range. That predicate is what makes the hole
/// unobservable. Refilling a slot that had to be CREATED would leave the accumulator carrying a member the read never
/// had (and, for a padded array index or a slice, a shape `set_component` would refuse or a deferred deletion would cut
/// back to the wrong length); declining hands those to the general read, which creates nothing. Everything the
/// predicate rejects — an absent key, an out-of-range index, a null or mismatched container, a slice, an index
/// SEARCH, a tagged container or component — is a path whose fold step was never the quadratic one.
///
/// A failure past the predicate is allocation-class only, and it consumes the root: an assignment fold that raises
/// publishes nothing, so there is no accumulator left to restore.
///
/// Test-only convenience over [`vacate_path`] plus a hole relink; production callers use [`take_path_chain`].
#[cfg(test)]
pub fn take_path(root: Value, path: &Value, resources: &ResourceContext<'_>) -> Result<Vacated, EngineRunError> {
    match vacate_path(root, path, resources)? {
        VacateResult::Taken { value, chain } => Ok(Vacated::Taken {
            value,
            root: chain.relink_holes(resources)?,
        }),
        VacateResult::Declined(root) => Ok(Vacated::Declined(root)),
    }
}

/// Reads the value one path addresses by VACATING it, keeping the ancestor chain so a later write refills by recorded
/// position.
///
/// The engine crate's assignment keeps the chain and writes through [`VacatedChain::write`] instead of walking the path
/// again, once that crate lands.
pub fn take_path_chain(
    root: Value,
    path: &Value,
    resources: &ResourceContext<'_>,
) -> Result<VacateResult, EngineRunError> {
    vacate_path(root, path, resources)
}

/// ONE descent decides eligibility and vacates as it goes, keeping the ancestor chain so a later write refills by
/// recorded position.
///
/// The existence predicate this walk used to run separately re-walked every container (one extra index lookup per
/// level, per fold step); folding it into the take keeps eligibility EXACTLY the vacating arms' own test — a level
/// that does not address an existing slot of an untagged container stops the walk, and the chain's recorded positions
/// relink what was already vacated without another lookup, so the decline still hands back a root the walk observably
/// never touched.
fn vacate_path(root: Value, path: &Value, resources: &ResourceContext<'_>) -> Result<VacateResult, EngineRunError> {
    let Value::Array(components) = path.untagged() else {
        return Ok(VacateResult::Declined(root));
    };
    if components.len() > depth::limit(Guarded::PathLength) {
        return Ok(VacateResult::Declined(root));
    }
    let mut chain: Vec<(Value, ComponentSlot)> = Vec::new();
    chain
        .try_reserve_exact(components.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    let mut current = root;
    for component in components {
        // Slot-addressing eligibility is the shared [`slot_pair`] predicate:
        // any other pair declines to the general read.
        let eligible = slot_pair(&current, component);
        let taken = if eligible {
            take_component(&mut current, component, resources)?
        } else {
            (Value::Null, None)
        };
        if let (child, Some(slot)) = taken {
            chain.push((current, slot));
            current = child;
        } else {
            // Not an already-existing slot: restore the vacated levels by position and decline, having observably
            // touched nothing.
            let mut restored = current;
            for (mut container, slot) in chain.into_iter().rev() {
                refill_slot(&mut container, &slot, restored, resources)?;
                restored = container;
            }
            return Ok(VacateResult::Declined(restored));
        }
    }
    Ok(VacateResult::Taken {
        value: current,
        chain: VacatedChain { levels: chain },
    })
}

/// The component list of a path value, rejecting a non-array path (`getpath("a")` is an error, not an empty walk) and a
/// path LONGER than the depth table's ceiling before the walk begins.
///
/// The length check is deliberately here, at the one place `getpath` and `setpath` read a caller's path array, and
/// deliberately BEFORE any component is followed: a 10001-component path against `null` is refused, without ever
/// touching the document.
fn path_components<'p>(path: &'p Value, resources: &ResourceContext<'_>) -> Result<&'p Array, EngineRunError> {
    match path.untagged() {
        Value::Array(array) => {
            if array.len() > depth::limit(Guarded::PathLength) {
                return Err(raise(depth::message(Guarded::PathLength), resources));
            }
            Ok(array)
        }
        _ => Err(raise("Path must be specified as an array", resources)),
    }
}

pub fn get_component(
    container: &Value,
    component: &Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    match (container.untagged(), component.untagged()) {
        (Value::Object(object), Value::String(name)) => match object.get(name) {
            Some(value) => Ok(try_clone(value)),
            None => crate::error::mismatch::resolve_at(
                resources,
                crate::error::mismatch::MismatchCell::PathMiss,
                false,
                Value::Null,
            ),
        },
        (Value::Array(array), Value::Number(number)) => match array_index(array.len(), number).position() {
            Some(position) => match array.get(position) {
                Some(value) => Ok(try_clone(value)),
                None => crate::error::mismatch::resolve_at(
                    resources,
                    crate::error::mismatch::MismatchCell::PathMiss,
                    false,
                    Value::Null,
                ),
            },
            None => crate::error::mismatch::resolve_at(
                resources,
                crate::error::mismatch::MismatchCell::PathMiss,
                false,
                Value::Null,
            ),
        },
        (Value::Array(array), Value::Array(needle)) => {
            crate::semantics::text::subsequence_indices(array, needle, resources)
        }
        (Value::Array(array), Value::Object(bounds)) => {
            let (start, end) = slice_bounds(bounds, array.len(), resources)?;
            slice_array(array, start, end, resources)
        }
        (Value::String(text), Value::Object(bounds)) => {
            let chars = text.chars().count();
            let (start, end) = slice_bounds(bounds, chars, resources)?;
            slice_string(text, start, end, resources)
        }
        (Value::Null, Value::String(_) | Value::Number(_) | Value::Object(_) | Value::Bytes(_)) => {
            crate::error::mismatch::resolve_at(
                resources,
                crate::error::mismatch::MismatchCell::PathMiss,
                false,
                Value::Null,
            )
        }
        (container, component) => Err(index_error(container, component, resources)),
    }
}

/// The child one component addresses, VACATED out of its slot.
///
/// This is [`get_component`] for an OWNED container, and it answers exactly what `get_component` answers — the
/// difference is only that the addressed slot is left holding `null` rather than a second handle on the child. That is
/// what makes the write back up the chain land in place: a child still named by the slot it came from is shared, and a
/// shared container detaches before it is written, which is the per-element rebuild this walk exists to avoid.
///
/// Leaving the hole is unobservable because `set_path` owns the whole chain: a completed walk writes the updated child
/// back into the same slot, and a failed one drops every container it vacated along with the error. Only the two
/// SLOT-addressing pairs take this route; an index search, a slice, and a tag wrapper name no single slot, so they read
/// through `get_component` as before.
/// Whether `(container, component)` is one of the two SLOT-addressing pairs — an object addressed by a string key, or
/// an array addressed by a number index. The eligibility gate ([`vacate_path`]) and the take dispatch
/// ([`take_component`]) must answer identically, so both read this one predicate.
fn slot_pair(container: &Value, component: &Value) -> bool {
    matches!(
        (container, component.untagged()),
        (Value::Object(_), Value::String(_)) | (Value::Array(_), Value::Number(_))
    )
}

fn take_component(
    container: &mut Value,
    component: &Value,
    resources: &ResourceContext<'_>,
) -> Result<(Value, Option<ComponentSlot>), EngineRunError> {
    if !slot_pair(container, component) {
        return get_component(container, component, resources).map(|value| (value, None));
    }
    let slot = match (&*container, component.untagged()) {
        (Value::Object(object), Value::String(name)) => match object.key_position(name) {
            Some(position) => ComponentSlot::Member(position),
            None => return Ok((Value::Null, None)),
        },
        (Value::Array(array), Value::Number(number)) => match array_position(array.len(), number) {
            Some(position) => ComponentSlot::Element(position),
            None => return Ok((Value::Null, None)),
        },
        // [`slot_pair`] admitted one of the two arms above.
        _ => return Err(internal("container kind changed under a path walk")),
    };
    let vacated = match (container, &slot) {
        (Value::Object(object), ComponentSlot::Member(position)) => object.try_get_index_mut(*position),
        (Value::Array(array), ComponentSlot::Element(position)) => array.try_get_mut(*position),
        _ => return Err(internal("container kind changed under a path walk")),
    }
    .map_err(|_| EngineRunError::allocation_failure())?;
    Ok(match vacated {
        Some(hole) => (core::mem::replace(hole, Value::Null), Some(slot)),
        None => (Value::Null, None),
    })
}

/// Writes `new` into the slot a descent RECORDED, without a second lookup.
///
/// The container is one the same walk's [`take_component`] just vacated at `slot` and nothing reshaped it in between
/// — its entry table or element spine still holds the hole at that position — so the position is the whole address.
/// This is the write half of both rebuild loops ([`set_path`]'s and [`VacatedChain::write`]'s): a keyed re-lookup here
/// was one of the five index walks every histogram fold step paid.
fn refill_slot(
    container: &mut Value,
    slot: &ComponentSlot,
    new: Value,
    _resources: &ResourceContext<'_>,
) -> Result<(), EngineRunError> {
    let hole = match (container, slot) {
        (Value::Object(object), ComponentSlot::Member(position)) => object.try_get_index_mut(*position),
        (Value::Array(array), ComponentSlot::Element(position)) => array.try_get_mut(*position),
        _ => return Err(internal("container kind changed under a path walk")),
    }
    .map_err(|_| EngineRunError::allocation_failure())?;
    match hole {
        Some(hole) => {
            *hole = new;
            Ok(())
        }
        None => Err(internal("a recorded path slot vanished under the rebuild")),
    }
}

/// The one slot a component addresses in an owned container, resolved before the entry table or element spine is
/// borrowed mutably — a detaching borrow moves the storage, and a position survives that move while a borrow would
/// not.
enum ComponentSlot {
    /// An object member, by insertion index.
    Member(usize),
    /// An array element, by resolved position.
    Element(usize),
}

///
/// A `null` container is EXTENDED into the container class the component names — a string key builds an object, a
/// number or slice builds an array — which is the whole of the path-creation rule.
///
/// The container arrives OWNED, so a slot write reaches storage through the `try_*_mut` copy-on-write gate: sole
/// ownership writes through, and a spine a twin still names detaches once. Neither branch is observable — they differ
/// only in how many allocations happened.
///
/// A TAGGED container keeps its tag: the path-update law says a path update retains the container's tag, and a write
/// into the payload is exactly that — rewriting one child (or a slice) does not change the node the tag belongs to.
/// Only an update VALUE computed by an explicit operation that already dropped the tag (`+`, arithmetic) arrives
/// untagged, and it is then stored as-is at the path.
fn set_component(
    container: Value,
    component: &Value,
    new: Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let Value::Tagged { tag, payload } = container else {
        return set_component_payload(container, component, new, resources);
    };
    let tag = tag.clone();
    let inner = (*payload).clone();
    let rewritten = set_component_payload(inner, component, new, resources)?;
    Value::try_tagged(tag, rewritten).map_err(|_| EngineRunError::allocation_failure())
}

/// The untagged half of [`set_component`]: the container is never a `Value::Tagged` here, so the container-class
/// dispatch is exact.
fn set_component_payload(
    container: Value,
    component: &Value,
    new: Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    // The container side is matched as its COPIED kind, not as a borrow of the value: every writable arm MOVES the
    // container into the mutation, and a borrow taken by the scrutinee would outlive the arm that consumes it.
    let is_null = matches!(container.kind(), ValueKind::Null);
    // The assignment-vivification cell: a write through null (or a missing member / past-the-end index, at
    // [`set_member`] and [`set_element`]) CREATES the container the path law extends. The through-null half of the
    // event is observed here, per component class.
    if is_null {
        crate::error::mismatch::resolve_at(
            resources,
            crate::error::mismatch::MismatchCell::AssignmentVivifies,
            false,
            (),
        )?;
    }
    match (container.kind(), component.untagged()) {
        (ValueKind::Object | ValueKind::Null, Value::String(name)) => {
            let object = owned_object(container, is_null, resources)?;
            set_member(object, name, new, resources)
        }
        (ValueKind::Array | ValueKind::Null, Value::Number(number)) => {
            if order::to_f64(number).is_nan() {
                return Err(raise("Cannot set array element at NaN index", resources));
            }
            let position = match array_index(container_len(&container), number) {
                ArrayIndex::At(position) => position,
                ArrayIndex::TooLarge => {
                    return Err(raise("Array index too large", resources));
                }
                ArrayIndex::BelowStart => {
                    return Err(raise("Out of bounds negative array index", resources));
                }
            };
            if position > WRITE_INDEX_MAX {
                return Err(raise("Array index too large", resources));
            }
            let array = owned_array(container, is_null, resources)?;
            set_element(array, position, new, resources)
        }
        (ValueKind::Array | ValueKind::Null, Value::Object(bounds)) => {
            let (start, end) = slice_bounds(bounds, container_len(&container), resources)?;
            // The bare form only: a TAGGED array is not spliced taglessly — destructuring it would silently drop the
            // tag the update law says path updates retain (`set_element`/`set_member` store the tagged value whole).
            let Value::Array(replacement) = new else {
                return Err(raise(
                    "A slice of an array can only be assigned another array",
                    resources,
                ));
            };
            let array = owned_array(container, is_null, resources)?;
            splice_array(&array, start, end, &replacement, resources)
        }
        (ValueKind::String, Value::Object(_)) => Err(raise("Cannot update string slices", resources)),
        (_, component) => Err(update_error(&container, component, resources)),
    }
}

/// The element count a number or slice component resolves against, which is zero for the `null` a set is about to
/// extend into an array.
fn container_len(container: &Value) -> usize {
    match container.untagged() {
        Value::Array(array) => array.len(),
        _ => 0,
    }
}

/// The object a set writes into: moved out of the container, or a fresh one when the null-extension rule builds it.
///
/// The container is never a `Value::Tagged` here — [`set_component`] peels the tag wrapper and re-wraps the result,
/// so this half works on the payload's own owned kind and the write reaches its storage directly.
fn owned_object(container: Value, is_null: bool, _resources: &ResourceContext<'_>) -> Result<Object, EngineRunError> {
    if is_null {
        return Object::try_new().map_err(|_| EngineRunError::allocation_failure());
    }
    match container {
        Value::Object(object) => Ok(object),
        _ => Err(internal("object update over a non-object")),
    }
}

/// The array a set descends into: the container itself, or a fresh one when the null-extension rule builds it.
fn owned_array(container: Value, is_null: bool, _resources: &ResourceContext<'_>) -> Result<Array, EngineRunError> {
    if is_null {
        return Array::try_new().map_err(|_| EngineRunError::allocation_failure());
    }
    match container {
        Value::Array(array) => Ok(array),
        _ => Err(internal("array update over a non-array")),
    }
}

/// One object with `name` set to `new`, preserving first-insertion order.
///
/// An existing key keeps its position and a new one appends, which is also the storage's rule: the value slot is
/// overwritten where it sits instead of the whole entry table being rebuilt around it.
fn set_member(
    mut object: Object,
    name: &str,
    new: Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    if let Some(position) = object.key_position(name) {
        let slot = object
            .try_get_index_mut(position)
            .map_err(|_| EngineRunError::allocation_failure())?
            .ok_or_else(|| internal("object member vanished under a path write"))?;
        *slot = new;
    } else {
        crate::error::mismatch::resolve_at(
            resources,
            crate::error::mismatch::MismatchCell::AssignmentVivifies,
            false,
            (),
        )?;
        object
            .try_insert_unique(clone_key(name)?, new)
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    Ok(Value::Object(object))
}

/// One array with `position` set to `new`, null-padded when it lies past the end.
fn set_element(
    mut array: Array,
    position: usize,
    new: Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    if let Some(missing) = (position + 1).checked_sub(array.len()).filter(|gap| *gap > 0) {
        crate::error::mismatch::resolve_at(
            resources,
            crate::error::mismatch::MismatchCell::AssignmentVivifies,
            false,
            (),
        )?;
        array
            .try_extend_null(missing)
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    let slot = array
        .try_get_mut(position)
        .map_err(|_| EngineRunError::allocation_failure())?
        .ok_or_else(|| internal("array element vanished under a path write"))?;
    *slot = new;
    Ok(Value::Array(array))
}

/// One array with `[start, end)` replaced by `replacement`'s elements.
fn splice_array(
    array: &Array,
    start: usize,
    end: usize,
    replacement: &Array,
    _resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let kept = array.len() - (end - start);
    let mut values = Vec::new();
    values
        .try_reserve_exact(kept + replacement.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for index in 0..start {
        values.push(clone_at(array, index)?);
    }
    for value in replacement {
        values.push(try_clone(value));
    }
    for index in end..array.len() {
        values.push(clone_at(array, index)?);
    }
    Array::try_from_vec(values)
        .map(Value::Array)
        .map_err(|_| EngineRunError::allocation_failure())
}

/// The sub-array `[start, end)`.
fn slice_array(
    array: &Array,
    start: usize,
    end: usize,
    _resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(end - start)
        .map_err(|_| EngineRunError::allocation_failure())?;
    for index in start..end {
        values.push(clone_at(array, index)?);
    }
    Array::try_from_vec(values)
        .map(Value::Array)
        .map_err(|_| EngineRunError::allocation_failure())
}

/// The sub-string `[start, end)`, counted in CHARACTERS.
fn slice_string(
    text: &str,
    start: usize,
    end: usize,
    _resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let mut out = String::new();
    for (index, character) in text.chars().enumerate() {
        if index >= end {
            break;
        }
        if index >= start {
            out.try_reserve(character.len_utf8())
                .map_err(|_| EngineRunError::allocation_failure())?;
            out.push(character);
        }
    }
    Value::try_string(&out).map_err(|_| EngineRunError::allocation_failure())
}

/// One element of `array`, duplicated by retaining its allocation.
fn clone_at(array: &Array, index: usize) -> Result<Value, EngineRunError> {
    match array.get(index) {
        Some(value) => Ok(try_clone(value)),
        None => Err(internal("array element vanished under a path walk")),
    }
}

/// `.[ [x] ]`: every start position at which `needle` occurs in `array`.
///
/// The in-range position one number component addresses, or `None` when no element can be there (a read yields `null`,
/// a deletion is a no-op).
pub fn array_position(len: usize, number: &Number) -> Option<usize> {
    array_index(len, number).position()
}

/// One array position under the addressing law.
enum ArrayIndex {
    /// An in-range absolute position (a negative index already wrapped).
    At(usize),
    /// Outside the `int` range the law converts to.
    TooLarge,
    /// A negative index that wraps below zero.
    BelowStart,
}

impl ArrayIndex {
    /// The position a READ addresses, `None` when no element can be there.
    const fn position(&self) -> Option<usize> {
        match self {
            Self::At(position) => Some(*position),
            Self::TooLarge | Self::BelowStart => None,
        }
    }
}

/// Resolves one number component against a container length.
///
/// The fractional part is TRUNCATED TOWARD ZERO before wrapping — the `(int)` conversion: `-0.5` and `0.9` both
/// address position `0`.
fn array_index(len: usize, number: &Number) -> ArrayIndex {
    let raw = order::to_f64(number);
    if !(INDEX_MIN..=INDEX_MAX).contains(&raw) {
        return ArrayIndex::TooLarge;
    }
    let truncated = if raw < 0.0 {
        ceil_binary64(raw)
    } else {
        floor_binary64(raw)
    };
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the range test above bounds the value to the `int` range"
    )]
    let index = truncated as i64;
    if index >= 0 {
        return usize::try_from(index).map_or(ArrayIndex::TooLarge, ArrayIndex::At);
    }
    let Ok(length) = i64::try_from(len) else {
        return ArrayIndex::TooLarge;
    };
    let wrapped = length + index;
    if wrapped < 0 {
        return ArrayIndex::BelowStart;
    }
    usize::try_from(wrapped).map_or(ArrayIndex::TooLarge, ArrayIndex::At)
}

/// Resolves a `{"start":…,"end":…}` component against a container length.
///
/// BOTH members must be present — the bounds are read by name and a missing one is rejected exactly as a non-numeric
/// one is — and each must be a number or `null`. Extra members are ignored. The bounds then follow the one slice law:
/// strictly-negative values are len-relative, the start rounds down, the end rounds up, both clamp to `[0, len]`, and a
/// backwards span collapses.
pub fn slice_bounds(
    bounds: &Object,
    len: usize,
    resources: &ResourceContext<'_>,
) -> Result<(usize, usize), EngineRunError> {
    let start = slice_bound(bounds, SLICE_START, len, true, resources)?;
    let end = slice_bound(bounds, SLICE_END, len, false, resources)?;
    Ok(if end <= start { (start, start) } else { (start, end) })
}

/// One resolved slice bound, or the slice-bound rejection.
fn slice_bound(
    bounds: &Object,
    name: &str,
    len: usize,
    is_start: bool,
    resources: &ResourceContext<'_>,
) -> Result<usize, EngineRunError> {
    let Some(bound) = bounds.get(name) else {
        return Err(raise(SLICE_INDICES, resources));
    };
    match bound.untagged() {
        Value::Null => Ok(if is_start { 0 } else { len }),
        Value::Number(number) => {
            let value = order::to_f64(number);
            // A NaN bound behaves like an OPEN bound, exactly as the step form's `owned_bound` resolves it — the
            // reference's law:
            // `[0,1,2,3,4] | .[nan:]` AND `.[:nan]` both answer the full array, and
            // `setpath([{"start":null,"end":nan}]; [9,9])` replaces the whole array. Resolving it here keeps the clamp
            // CONSULT predicate and the clamp TEST predicate below from ever seeing a NaN `rounded`, so the two
            // spellings cannot disagree.
            if value.is_nan() {
                return Ok(if is_start { 0 } else { len });
            }
            // The slice-clamp cell for the PATH walk's slice component: an authored bound whose len-relative value lies
            // OUTSIDE `[0, len]` clamped. The predicate is the step form's own (`stage.rs`'s `slice_span`): the
            // AUTHORED value, negative wrapping, tested with a strict `>` — a bound resolving exactly to the length
            // (`getpath([{"start":0,"end":3}])` over `[1,2]`) is the ordinary end, not a clamp, and a fractional bound
            // that rounds up to it is not either. `-0.0` is not a clamp; a NaN bound never reaches this point (it
            // resolves as open above).
            //
            // Deliberate asymmetry with the resolution below: the CONSULT tests the AUTHORED relative value, while
            // `resolve_bound` tests the ROUNDED one. An authored bound outside `[0, len]` consults even when its
            // rounded position lands inside — jq resolves such a bound silently, so under lenient (the default) the
            // cell is an observation only and the resolved answer stands.
            #[allow(
                clippy::cast_precision_loss,
                reason = "a container longer than 2^53 elements cannot exist in a decoded document"
            )]
            let relative = if value < 0.0 { len as f64 + value } else { value };
            #[allow(
                clippy::cast_precision_loss,
                reason = "a container longer than 2^53 elements cannot exist in a decoded document"
            )]
            if relative < 0.0 || relative > len as f64 {
                crate::error::mismatch::resolve_at(
                    resources,
                    crate::error::mismatch::MismatchCell::SliceClamped,
                    false,
                    (),
                )?;
            }
            Ok(resolve_bound(value, len, is_start))
        }
        _ => Err(raise(SLICE_INDICES, resources)),
    }
}

/// The slice-bound rejection, whose text carries no operand at all.
const SLICE_INDICES: &str = "Array/string slice indices must be integers";

/// The len-relative ROUNDED value of one bound, under the authored-sign law (start floors, end ceils). This derivation
/// — negative wrapping plus the rounding direction — is the half the clamp CONSULT predicate above shares; the
/// value each predicate TESTS differs by design (consult: authored, resolution: rounded), which is why no single helper
/// can carry both.
#[allow(
    clippy::cast_precision_loss,
    reason = "a container longer than 2^53 elements cannot exist in a decoded document"
)]
fn rounded_bound(value: f64, len: usize, is_start: bool) -> f64 {
    let length = len as f64;
    let relative = if value < 0.0 { length + value } else { value };
    if is_start {
        floor_binary64(relative)
    } else {
        ceil_binary64(relative)
    }
}

/// The container position one slice bound denotes, under the len-relative authored-sign law the `.[a:b]` step already
/// applies.
#[allow(
    clippy::cast_precision_loss,
    reason = "a container longer than 2^53 elements cannot exist in a decoded document"
)]
fn resolve_bound(value: f64, len: usize, is_start: bool) -> usize {
    let rounded = rounded_bound(value, len, is_start);
    if rounded
        .partial_cmp(&0.0)
        .is_none_or(|order| order != core::cmp::Ordering::Greater)
    {
        0
    } else if rounded >= len as f64 {
        len
    } else {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the branches above bound `rounded` to the open interval (0, len)"
        )]
        let position = rounded as usize;
        position
    }
}

/// The index-class rejection for one `(container, component)` pair.
pub fn index_error(container: &Value, component: &Value, resources: &ResourceContext<'_>) -> EngineRunError {
    let rendered = match message::render_owned_key(component) {
        Ok(rendered) => rendered,
        Err(error) => return error,
    };
    match message::index_message(container.kind(), &rendered) {
        Ok(text) => raise(&text, resources),
        Err(error) => error,
    }
}

/// The update-class rejection: the pair is addressable but not WRITABLE, which is a different sentence from the read
/// rejection (`setpath([[0]];9)` over an array reports the update class, because the read of an array with an array
/// component succeeds — it is the index search).
fn update_error(container: &Value, component: &Value, resources: &ResourceContext<'_>) -> EngineRunError {
    match message::update_field_message(component.kind(), container.kind()) {
        Ok(text) => raise(&text, resources),
        Err(error) => error,
    }
}

/// The `Invalid path expression with result <v>`: a path-mode subgraph produced a value that is not the value at the
/// tracked path.
pub fn invalid_result(value: &Value, resources: &ResourceContext<'_>) -> EngineRunError {
    let rendered = match message::dump_trunc_owned(value) {
        Ok(rendered) => rendered,
        Err(error) => return error,
    };
    match message::invalid_path_result_message(&rendered) {
        Ok(text) => raise(&text, resources),
        Err(error) => error,
    }
}

/// The `Invalid path expression near attempt to access element <k> of <v>`: a navigation step ran over a value the
/// register does not track.
pub fn invalid_access(component: &Value, container: &Value, resources: &ResourceContext<'_>) -> EngineRunError {
    let key = match message::dump_trunc_owned(component) {
        Ok(key) => key,
        Err(error) => return error,
    };
    let subject = match message::dump_trunc_owned(container) {
        Ok(subject) => subject,
        Err(error) => return error,
    };
    match message::invalid_path_access_message(&key, &subject) {
        Ok(text) => raise(&text, resources),
        Err(error) => error,
    }
}

/// The `Invalid path expression near attempt to iterate through <v>`: `.[]` ran over a value the register does not
/// track.
///
/// Iteration gets its own sentence because it names no component — there is no single element it was trying to reach.
pub fn invalid_iterate(container: &Value, resources: &ResourceContext<'_>) -> EngineRunError {
    let subject = match message::dump_trunc_owned(container) {
        Ok(subject) => subject,
        Err(error) => return error,
    };
    match message::invalid_path_iterate_message(&subject) {
        Ok(text) => raise(&text, resources),
        Err(error) => error,
    }
}

/// Raises one path-family message as an ordinary catchable error VALUE.
///
/// Path errors are plain string errors: `try … catch .` sees the sentence, and an uncaught one renders through the
/// same `jq: error (at …): <text>` line every raised string takes. Modelling them as new typed variants would buy a
/// second rendering path for text that is already final.
pub fn raise(message: &str, _resources: &ResourceContext<'_>) -> EngineRunError {
    Value::try_string(message).map_or_else(|_| EngineRunError::allocation_failure(), EngineRunError::Raised)
}

/// Duplicates one owned value by retaining its allocations.
pub fn try_clone(value: &Value) -> Value {
    value.clone()
}

/// Fallibly allocates one shared object key.
pub fn clone_key(text: &str) -> Result<jqf_data::ObjectKey, EngineRunError> {
    jqf_data::ObjectKey::try_from_str(text).map_err(|_| EngineRunError::allocation_failure())
}

/// Fallibly allocates the shared text one register step holds for a key.
///
/// The bytes are charged HERE, once per push, and every path that spells the step retains them instead of copying —
/// see [`PathStep::Key`], which also says why a step may not simply share the object entry's own key allocation. A push
/// site that already holds the key as a [`Value::String`] skips this and retains that value's allocation.
pub fn charged_key(text: &str, _resources: &ResourceContext<'_>) -> Result<Shared<str>, EngineRunError> {
    Shared::try_from_str(text).map_err(|_| EngineRunError::allocation_failure())
}

/// Slots in one run's key cache. A power of two, so the index is a mask.
///
/// The cache exists to collapse REPETITION, not to index a document. A record-shaped input repeats one small key set
/// across every row, and the table holds that set outright. A document whose keys are DATA — `{"k0000001": …,
/// "k0000002": …}` — repeats nothing, misses every time, and must not pay for a table that grows with it: direct
/// mapping keeps that worst case at one hash and one failed comparison over the allocation it was going to make anyway.
///
/// 1024 keeps the worst case at one hash and one failed comparison over the allocation the miss was going to make
/// anyway. The table costs 8 KiB of slots however many keys a document has, while a document with a KEY SET that
/// outgrows it thrashes and loses the whole win. Only the slots a document actually touches are ever read, so a wide
/// table does not cost the 13-key case its locality.
const KEY_CACHE_SLOTS: usize = 1024;

/// The greatest number of leading bytes the slot hash reads.
///
/// A hash is only a slot GUESS — the full text is compared before any handle is reused — so it may read a prefix
/// and stay correct. Bounding it keeps a pathological key length off the per-visit cost.
const KEY_HASH_BYTES: usize = 16;

/// The shared key texts one path-mode run has already paid for.
///
/// [`charged_key`]'s contract is that a step's text carries its OWN residency, which is why a step may not borrow the
/// object entry's key allocation. Read literally that forces an allocation per object-member VISIT — and a walk that
/// emits paths visits every member of every sibling object, so `paths` over a 350k-row document allocated one copy of
/// `"id"` per row and retained all of them, because every emitted path beneath a member holds a handle on that member's
/// step text.
///
/// The contract only ever required an allocation the RUN owns, not a fresh one per visit. This cache is that
/// allocation: one charge per distinct text among the keys currently resident, held for the run and handed to every
/// sibling spelling the same key as an ordinary refcount bump. The emitted `Value::String` is byte-identical either way
/// — only the allocation's IDENTITY changes, and no projection can observe that.
///
/// It is DIRECT-MAPPED rather than a growing table: an ordered table would answer a hit with string comparisons that
/// cost more than the allocation they replace. One slot per hash costs one comparison on the hit and one on the miss,
/// which is cheap enough that the key-diverse worst case stays flat.
///
/// A collision simply EVICTS. Correctness never rests on the hash: a slot's text is compared in full before its handle
/// is reused, so a collision costs a fresh allocation — exactly what the site did before this cache existed — and
/// never a wrong key.
///
/// Negative space: it is not a document-wide string interner. It holds only the texts a path STEP spelled, it never
/// sees a value's text, and it is scoped to one run so a long request cannot accumulate keys across unrelated inputs.
#[derive(Debug, Default)]
pub struct KeyIntern {
    /// The FIRST distinct text this run interned, held outside the table.
    ///
    /// An assignment run (`.[$r.bucket] += 1`, `.[$u.tier] = true`) interns exactly one distinct key, and the engine
    /// crate's fold will open one run per step once that crate lands — so a 50k-step reduce would pay the 1024-slot
    /// table's `resize_with` and its element-walking drop once PER STEP. Holding the first text inline keeps the
    /// single-key run's whole lifecycle at one `Option`: the table below is not allocated until a SECOND distinct text
    /// arrives.
    first: Option<Shared<str>>,
    /// One held text per slot, empty until a second distinct key is interned.
    slots: Vec<Option<Shared<str>>>,
}

impl KeyIntern {
    /// The shared text for `name`, reusing this run's allocation when the slot already holds that exact key and
    /// charging a new one when it does not.
    pub fn intern(&mut self, name: &str, resources: &ResourceContext<'_>) -> Result<Shared<str>, EngineRunError> {
        // The single-key fast path: one comparison against the held first text, and no table until a second distinct
        // key forces one.
        match &self.first {
            Some(held) if &**held == name => return Ok(held.clone_shared()),
            None => {
                let text = charged_key(name, resources)?;
                self.first = Some(text.clone_shared());
                return Ok(text);
            }
            Some(_) => {}
        }
        if self.slots.is_empty() {
            self.slots
                .try_reserve_exact(KEY_CACHE_SLOTS)
                .map_err(|_| EngineRunError::allocation_failure())?;
            self.slots.resize_with(KEY_CACHE_SLOTS, || None);
        }
        let slot = slot_of(name);
        if let Some(held) = &self.slots[slot]
            && &**held == name
        {
            return Ok(held.clone_shared());
        }
        let text = charged_key(name, resources)?;
        self.slots[slot] = Some(text.clone_shared());
        Ok(text)
    }
}

/// The cache slot one key name maps to: FNV-1a over its length and a bounded prefix, masked to [`KEY_CACHE_SLOTS`].
fn slot_of(name: &str) -> usize {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |value: u64| {
        hash ^= value;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    };
    // The length participates so that a shared prefix does not collapse `"id"` and `"identifier"` onto one slot in a
    // document that carries both.
    mix(u64::try_from(name.len()).unwrap_or(u64::MAX));
    for byte in name.as_bytes().iter().take(KEY_HASH_BYTES) {
        mix(u64::from(*byte));
    }
    let mask = u64::try_from(KEY_CACHE_SLOTS - 1).unwrap_or(0);
    usize::try_from(hash & mask).unwrap_or(0)
}

/// One integer-valued number.
fn integer(value: i64) -> Value {
    Value::Number(Number::integer(jqf_data::Integer::from_i64(value)))
}

/// A broken internal contract in the path walk.
fn internal(contract: &'static str) -> EngineRunError {
    EngineRunError::internal_contract(contract)
}

#[cfg(test)]
mod tests {
    use super::{
        KEY_CACHE_SLOTS, KeyIntern, PathStep, VacateResult, Vacated, charged_key, later_extensions, set_path,
        slice_bounds, step_value, take_path, vacate_path,
    };
    use crate::semantics::order;
    use alloc::vec;
    use alloc::vec::Vec;
    use jqf_data::{Array, Number, ObjectBuilder, ObjectKey, Value};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    /// One unlimited request ledger: a step's key text is charged on its own allocation, so no fixture builds without
    /// an account.
    fn ledger() -> ResourceContext<'static> {
        let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let account = RequestAccount::try_new(limits).expect("test account");
        let work = WorkMeter::try_new_v1(1).expect("test work meter");
        ResourceContext::new(account, &CONTROL, work).expect("test ledger")
    }

    /// A NaN slice bound resolves as OPEN at both ends instead of clamping to zero and emptying the slice; there is no
    /// `SliceClamped` consult.
    /// NaN resolves as open BEFORE the clamp predicates run, so the two can never disagree. The setpath spelling is
    /// pinned by the corpus rows.
    #[test]
    fn a_nan_slice_bound_is_open_for_both_ends() {
        let resources = ledger();
        let nan = Value::Number(Number::float(jqf_data::Float::new(f64::NAN)));
        let bounds = {
            let mut builder = ObjectBuilder::try_with_capacity(2).expect("builder");
            builder
                .try_insert_last(jqf_data::ObjectKey::try_from_str("start").expect("key"), nan.clone())
                .expect("insert");
            builder
                .try_insert_last(jqf_data::ObjectKey::try_from_str("end").expect("key"), nan.clone())
                .expect("insert");
            builder.try_finish().expect("finish")
        };
        let (start, end) = slice_bounds(&bounds, 5, &resources).expect("bounds resolve");
        assert_eq!(
            (start, end),
            (0, 5),
            "a NaN bound must behave like an open bound, not clamp to 0"
        );
    }

    /// A key step is spelled by RETAINING its text, never by copying it.
    ///
    /// This is the whole of the register's charge-once discipline: a step at depth `d` is spelled again for every path
    /// emitted from beneath it, so a rendering that allocated would restore the O(paths x depth) copies the shared step
    /// exists to remove — and would do it silently, since the emitted bytes are identical either way. Allocation
    /// identity is therefore what is asserted, through the text's data pointer: two handles on one allocation share it,
    /// and a copy cannot.
    #[test]
    fn a_key_step_emits_a_string_sharing_the_step_allocation() {
        let resources = ledger();
        // Empty, ASCII, multibyte, and a key carrying an escape-needing byte:
        // the emitted VALUE must be the key whatever its bytes are.
        for name in ["", "k", "ключ", "a\tb"] {
            let step = PathStep::Key(charged_key(name, &resources).expect("charged key text"));
            let PathStep::Key(held) = &step else {
                unreachable!("the step under test is a key");
            };
            let emitted = step_value(&step, &resources).expect("a key step renders");
            let Value::String(rendered) = &emitted else {
                panic!("a key step renders as a string");
            };
            assert_eq!(&**rendered, name, "emitted key text for {name:?}");
            assert!(
                core::ptr::eq(rendered.as_ptr(), held.as_ptr()),
                "the emitted string copied {name:?} instead of retaining it"
            );

            // The second rendering is the one the copy cost used to fall on: a step is spelled once per path beneath
            // it, not once in total.
            let again = step_value(&step, &resources).expect("a key step renders again");
            let Value::String(twin) = &again else {
                panic!("a key step renders as a string");
            };
            assert!(
                core::ptr::eq(twin.as_ptr(), held.as_ptr()),
                "re-rendering {name:?} copied it"
            );
        }
    }

    /// A repeated key is charged ONCE per run, and a collision is still correct.
    ///
    /// The walk visits every member of every sibling object, so the interesting assertion is the second visit to the
    /// same key: it must hand back the SAME allocation, which is the whole win. The rest of the case pins what the
    /// cache may never do — answer with a text that is not the key asked for — including for the shared-prefix pair
    /// the slot hash folds the length in to separate.
    #[test]
    fn a_repeated_key_reuses_one_run_allocation_and_never_answers_the_wrong_text() {
        let resources = ledger();
        let mut keys = KeyIntern::default();

        let first = keys.intern("id", &resources).expect("first intern");
        let second = keys.intern("id", &resources).expect("second intern");
        assert!(
            core::ptr::eq(first.as_ptr(), second.as_ptr()),
            "a repeated key allocated a second time"
        );

        // Every distinct text answers itself whatever slot it lands in. The prefix pair and the empty key are the
        // shapes the hash could confuse; a wider sweep than the table is deliberate, so eviction is exercised.
        let mut names = alloc::vec::Vec::new();
        for index in 0..(KEY_CACHE_SLOTS * 2) {
            names.push(alloc::format!("k{index}"));
        }
        for name in ["", "id", "identifier", "ключ", "a\tb"] {
            names.push(alloc::string::String::from(name));
        }
        for name in &names {
            let held = keys.intern(name, &resources).expect("intern under eviction");
            assert_eq!(&*held, name.as_str(), "the cache answered the wrong text");
        }
    }

    /// One integer-valued value.
    fn int_value(text: &str) -> Value {
        Value::Number(jqf_data::Number::try_json_literal(text).expect("integer"))
    }

    /// A `.[0] = …`-style write INTO a tagged array retains the tag on the container (AGENTS.md's path-update law),
    /// and the payload sees the write.
    #[test]
    fn a_write_into_a_tagged_container_retains_the_tag() {
        let resources = ledger();
        let mut array = jqf_data::Array::try_new().expect("array");
        array.try_push(int_value("1")).expect("push");
        array.try_push(int_value("2")).expect("push");
        let tag = jqf_data::TagId::try_new_unaccounted("!list").expect("tag");
        let tagged = Value::try_tagged(tag, Value::Array(array)).expect("tagged");
        let component = int_value("0");
        let new = int_value("9");
        let rewritten = super::set_component(tagged, &component, new, &resources).expect("set");
        let Value::Tagged { tag, payload } = &rewritten else {
            panic!("a write into a tagged container must keep the tag, got {rewritten:?}");
        };
        assert_eq!(tag.as_str(), "!list");
        let Value::Array(array) = (**payload).untagged() else {
            panic!("payload must stay an array");
        };
        let Some(Value::Number(first)) = array.get(0) else {
            panic!("payload must still have a first element");
        };
        assert_eq!(
            first.to_integer().expect("integer").as_str(),
            "9",
            "the write must land in the payload"
        );
    }

    fn path_array(components: Vec<Value>) -> Value {
        Value::Array(Array::try_from_vec(components).expect("path array"))
    }

    fn key(name: &str) -> Value {
        Value::try_string(name).expect("path key")
    }

    /// Whether each path is a strict prefix of some LATER path — order-dependent, borrowed, and leaf-only in the seen
    /// set. An earlier path that a later one extends must stay true; a later path that an earlier one extends must stay
    /// false; siblings and equal paths never count as extensions.
    #[test]
    fn later_extensions_short_circuits_on_the_first_later_leaf() {
        let resources = ledger();
        let empty = path_array(vec![]);
        let a = path_array(vec![key("a")]);
        let ab = path_array(vec![key("a"), key("b")]);
        let abc = path_array(vec![key("a"), key("b"), key("c")]);
        let ac = path_array(vec![key("a"), key("c")]);
        let b = path_array(vec![key("b")]);

        assert_eq!(
            later_extensions(&[], &resources).expect("empty set"),
            Vec::<bool>::new()
        );
        assert_eq!(
            later_extensions(core::slice::from_ref(&a), &resources).expect("singleton"),
            vec![false]
        );
        assert_eq!(
            later_extensions(&[a.clone(), ab.clone()], &resources).expect("prefix then leaf"),
            vec![true, false]
        );
        assert_eq!(
            later_extensions(&[ab.clone(), a.clone()], &resources).expect("leaf then earlier prefix"),
            vec![false, false],
            "an earlier path is not a later extension"
        );
        assert_eq!(
            later_extensions(&[a.clone(), b.clone()], &resources).expect("siblings"),
            vec![false, false]
        );
        assert_eq!(
            later_extensions(&[a.clone(), a.clone()], &resources).expect("equal paths"),
            vec![false, false],
            "equal paths are not strict prefixes"
        );
        assert_eq!(
            later_extensions(&[empty, a.clone()], &resources).expect("empty prefix"),
            vec![true, false]
        );
        assert_eq!(
            later_extensions(&[a.clone(), ab.clone(), abc.clone()], &resources).expect("nested chain"),
            vec![true, true, false],
            "each proper prefix of a later leaf is an extension, and the leaf is not inserted as a second witness"
        );
        assert_eq!(
            later_extensions(&[a, ab, ac], &resources).expect("branching later leaves"),
            vec![true, false, false]
        );
    }

    fn object_from(pairs: &[(&str, Value)]) -> Value {
        let mut builder = ObjectBuilder::try_with_capacity(pairs.len()).expect("builder");
        for (name, value) in pairs {
            builder
                .try_insert_last(ObjectKey::try_from_str(name).expect("key"), value.clone())
                .expect("insert");
        }
        Value::Object(builder.try_finish().expect("object"))
    }

    fn same(left: &Value, right: &Value) -> bool {
        order::semantic_eq(left, right).expect("comparison depth")
    }

    /// The descent keeps the vacated chain; writing through it must equal a fresh [`set_path`] walk over the same
    /// document, and the take-then-set sequence (relink the holes, then walk again) must produce the same assignment
    /// content.
    #[test]
    fn a_kept_vacated_chain_write_matches_set_path() {
        let resources = ledger();
        let inner = object_from(&[("c", int_value("1")), ("d", int_value("2"))]);
        let doc = object_from(&[("a", inner), ("b", int_value("0"))]);
        let path = path_array(vec![key("a"), key("c")]);
        let new = int_value("9");

        let walked = set_path(doc.clone(), &path, new.clone(), &resources).expect("set_path");
        let VacateResult::Taken { value, chain } = vacate_path(doc.clone(), &path, &resources).expect("vacate") else {
            panic!("existing object path must vacate");
        };
        assert!(
            same(&value, &int_value("1")),
            "vacate must hand over the addressed leaf"
        );
        let reused = chain.write(new.clone(), &resources).expect("chain write");
        assert!(
            same(&walked, &reused),
            "refilling the recorded slots must match a fresh path walk"
        );

        let Vacated::Taken { value: taken, root } = take_path(doc, &path, &resources).expect("take_path") else {
            panic!("existing object path must take");
        };
        assert!(same(&taken, &int_value("1")));
        let through_holes = set_path(root, &path, new, &resources).expect("set on holed root");
        assert!(
            same(&walked, &through_holes),
            "take then set must keep assignment content"
        );
    }

    /// Array indices are the other slot class the vacating descent records.
    #[test]
    fn a_kept_array_chain_write_matches_set_path() {
        let resources = ledger();
        let row = object_from(&[("score", int_value("3"))]);
        let doc =
            Value::Array(Array::try_from_vec(vec![row, object_from(&[("score", int_value("4"))])]).expect("array"));
        let path = path_array(vec![int_value("0"), key("score")]);
        let new = int_value("7");

        let walked = set_path(doc.clone(), &path, new.clone(), &resources).expect("set_path");
        let VacateResult::Taken { chain, .. } = vacate_path(doc, &path, &resources).expect("vacate") else {
            panic!("existing array path must vacate");
        };
        let reused = chain.write(new, &resources).expect("chain write");
        assert!(same(&walked, &reused), "array-slot refill must match a fresh path walk");
    }

    /// An empty path vacates the whole document: the chain is empty and the write is the new value, which is also what
    /// [`set_path`] of `[]` answers.
    #[test]
    fn an_empty_path_chain_write_replaces_the_root() {
        let resources = ledger();
        let doc = object_from(&[("a", int_value("1"))]);
        let path = path_array(vec![]);
        let new = int_value("9");
        let walked = set_path(doc.clone(), &path, new.clone(), &resources).expect("set_path");
        let VacateResult::Taken { value, chain } = vacate_path(doc, &path, &resources).expect("vacate") else {
            panic!("empty path must take the root");
        };
        assert!(same(&value, &object_from(&[("a", int_value("1"))])));
        let reused = chain.write(new, &resources).expect("chain write");
        assert!(same(&walked, &reused));
        assert!(same(&walked, &int_value("9")));
    }

    /// A missing member declines, restores the document, and leaves [`set_path`] to vivify — the chain is not kept
    /// because no slot was vacated.
    #[test]
    fn a_missing_member_declines_without_a_chain() {
        let resources = ledger();
        let doc = object_from(&[("a", int_value("1"))]);
        let path = path_array(vec![key("missing")]);
        let VacateResult::Declined(restored) = vacate_path(doc.clone(), &path, &resources).expect("vacate") else {
            panic!("absent key must decline");
        };
        assert!(same(&doc, &restored));
        let written = set_path(restored, &path, int_value("2"), &resources).expect("vivify");
        assert!(same(
            &written,
            &object_from(&[("a", int_value("1")), ("missing", int_value("2"))])
        ));
    }
}
