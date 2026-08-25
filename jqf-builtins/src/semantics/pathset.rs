//! The deletion law: many paths deleted from one value, at once.
//!
//! One job: apply a SET of paths to a value as a single structural edit. Deleting paths one at a time is not the same
//! operation — every deletion shifts the positions the remaining paths address, and the answer is the one a
//! simultaneous edit gives: `[1,2,3,4,5] | delpaths([[{"start":1,"end":3}],[2]])` deletes ORIGINAL positions 1, 2 and
//! 2, yielding `[1,4,5]`, not the `[1,4]` a left-to-right sequence of deletions would give.
//!
//! The shape that answers it is a TRIE keyed by path component ([`DeleteTree`]):
//! duplicate paths collapse into one node, a path that is a PREFIX of another swallows it (a node that terminates
//! deletes its whole subtree, so `delpaths([[0],[0,0]])` never tries to index into the element it removed), and each
//! container is rebuilt bottom-up. Within one array level the walk follows the reference's sorted-delete law:
//! non-terminal components rebuild their position or range in a WORKING array as the walk proceeds, so a later
//! component re-resolves against the array as it now stands, while the terminal keys are deferred and applied once,
//! simultaneously, against the final array — see [`apply_to_array`].
//!
//! Negative space: it reads no path expression, evaluates no subgraph, and holds no resource ledger of its own —
//! every allocation is charged through the caller's [`ResourceContext`].

use alloc::vec::Vec;
use core::cmp::Ordering;

use jqf_data::{Array, Object, ObjectBuilder, Value};
use jqf_resource::ResourceContext;

use crate::error::EngineRunError;
use crate::error::message;
use crate::semantics::depth::{self, Guarded, TooDeep, comparison_error};
use crate::semantics::order;
use crate::semantics::owned_kind;
use crate::semantics::path::{array_position, raise, slice_bounds, try_clone};

/// A trie of the paths one `delpaths` removes.
#[derive(Default)]
pub struct DeleteTree {
    /// Whether a path ENDS at this node, which deletes the value it addresses — and with it the whole subtree below,
    /// so the children become unreachable.
    terminal: bool,
    /// One entry per DISTINCT component at this level, in path order.
    entries: Vec<Branch>,
}

/// One component of the trie and everything the paths through it delete.
struct Branch {
    /// The component value, as the caller's path array spelled it.
    component: Value,
    /// The paths that reach or pass through this component.
    children: DeleteTree,
}

impl DeleteTree {
    /// Builds the trie for a whole path SET in one sorted pass.
    ///
    /// Sorting first is what keeps the build linearithmic. A trie grown one path at a time has to SEARCH each level for
    /// a component it may already hold, which costs O(k²) comparisons over k sibling paths — a 10,000-path
    /// `delpaths` feels that quadratic directly. Sorted paths instead arrive already grouped: every level is written
    /// once, left to right, and the only question each path answers is how much of the previous path's spine it still
    /// shares.
    fn build(paths: &Array, resources: &ResourceContext<'_>) -> Result<Self, EngineRunError> {
        let mut sorted = Vec::new();
        sorted
            .try_reserve_exact(paths.len())
            .map_err(|_| EngineRunError::allocation_failure())?;
        for path in paths {
            let Value::Array(path) = path.untagged() else {
                return Err(not_a_path(path, resources));
            };
            if path.len() > depth::limit(Guarded::PathLength) {
                return Err(raise(depth::message(Guarded::PathLength), resources));
            }
            sorted.push(path);
        }
        // A component pair the total order refuses to compare cannot be ordered into a made-up `Equal` — that would
        // merge two distinct branches of the trie and delete the wrong positions. The flag turns the comparator's
        // decline into the sort's error.
        let mut too_deep = false;
        sorted.sort_unstable_by(|left, right| {
            compare_paths(left, right).unwrap_or_else(|TooDeep| {
                too_deep = true;
                Ordering::Equal
            })
        });
        if too_deep {
            return Err(comparison_error(resources));
        }

        let mut root = Self::default();
        let mut spine: Vec<Branch> = Vec::new();
        let mut previous: Option<&Array> = None;
        for path in &sorted {
            let shared = match previous {
                Some(previous) => shared_prefix(previous, path).map_err(|TooDeep| comparison_error(resources))?,
                None => 0,
            };
            while spine.len() > shared {
                let Some(branch) = spine.pop() else {
                    return Err(internal("a delete-tree spine underflowed"));
                };
                close(&mut spine, &mut root, branch)?;
            }
            for depth in shared..path.len() {
                let Some(component) = path.get(depth) else {
                    return Err(internal("a delete-tree path shrank mid-build"));
                };
                spine.try_reserve(1).map_err(|_| EngineRunError::allocation_failure())?;
                spine.push(Branch {
                    component: try_clone(component),
                    children: Self::default(),
                });
            }
            // An EMPTY path leaves the spine empty and terminates the ROOT: it addresses the whole value, and deleting
            // the whole value is `null`.
            match spine.last_mut() {
                Some(branch) => branch.children.terminal = true,
                None => root.terminal = true,
            }
            previous = Some(path);
        }
        while let Some(branch) = spine.pop() {
            close(&mut spine, &mut root, branch)?;
        }
        Ok(root)
    }

    /// Whether this level deletes nothing at all.
    fn is_empty(&self) -> bool {
        self.entries.is_empty() && !self.terminal
    }
}

/// Attaches one finished branch to whatever level is still open above it.
fn close(spine: &mut [Branch], root: &mut DeleteTree, branch: Branch) -> Result<(), EngineRunError> {
    let level = match spine.last_mut() {
        Some(parent) => &mut parent.children,
        None => root,
    };
    level
        .entries
        .try_reserve(1)
        .map_err(|_| EngineRunError::allocation_failure())?;
    level.entries.push(branch);
    Ok(())
}

/// How many leading components two paths share.
fn shared_prefix(left: &Array, right: &Array) -> Result<usize, TooDeep> {
    let mut shared = 0;
    loop {
        match (left.get(shared), right.get(shared)) {
            (Some(left), Some(right)) if order::total_cmp(left, right)?.is_eq() => {
                shared += 1;
            }
            _ => return Ok(shared),
        }
    }
}

/// Orders two paths lexicographically, a PREFIX sorting before what extends it.
///
/// The prefix rule is what lets the build treat a swallowed path as a plain continuation of the spine: `[0]` arrives
/// before `[0,0]`, marks its node terminal, and the deeper path then hangs off a node already known to be deleted.
fn compare_paths(left: &Array, right: &Array) -> Result<Ordering, TooDeep> {
    let shared = shared_prefix(left, right)?;
    match (left.get(shared), right.get(shared)) {
        (Some(left), Some(right)) => order::total_cmp(left, right),
        (Some(_), None) => Ok(Ordering::Greater),
        (None, Some(_)) => Ok(Ordering::Less),
        (None, None) => Ok(Ordering::Equal),
    }
}

/// The `delpaths` law: `root` with every path in `paths` removed simultaneously.
pub fn delete_paths(root: &Value, paths: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let Value::Array(paths) = paths.untagged() else {
        return Err(raise("Paths must be specified as an array", resources));
    };
    let tree = DeleteTree::build(paths, resources)?;
    if tree.terminal {
        return Ok(Value::Null);
    }
    apply(&tree, try_clone(root), resources)
}

/// The per-element path rejection, which names the offending element's KIND (`Path must be specified as array, not
/// string`) — a different sentence from the whole-argument rejection above it.
fn not_a_path(path: &Value, resources: &ResourceContext<'_>) -> EngineRunError {
    match message::not_an_array_path_message(owned_kind(path)) {
        Ok(text) => raise(&text, resources),
        Err(error) => error,
    }
}

/// Rebuilds `value` with this level's deletions applied.
///
/// A `null` absorbs every deletion unchanged, exactly as `null | del(.a)` does:
/// there is nothing to remove and nothing to create.
///
/// A TAGGED container keeps its tag, by AGENTS.md's path-update law: deleting a member of a tagged object (or an
/// element of a tagged array) rewrites the payload, not the node the tag belongs to.
fn apply(tree: &DeleteTree, value: Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    if tree.is_empty() {
        return Ok(value);
    }
    let Value::Tagged { tag, payload } = value else {
        return apply_payload(tree, value, resources);
    };
    let tag = tag.clone();
    let inner = (*payload).clone();
    let rebuilt = apply_payload(tree, inner, resources)?;
    Value::try_tagged(tag, rebuilt).map_err(|_| EngineRunError::allocation_failure())
}

/// The untagged half of [`apply`]: `value` is never a `Value::Tagged` here.
fn apply_payload(tree: &DeleteTree, value: Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    match value.untagged() {
        Value::Null => Ok(value),
        Value::Object(object) => apply_to_object(tree, object, resources),
        Value::Array(array) => apply_to_array(tree, array, resources),
        other => Err(cannot_delete_from(other, resources)),
    }
}

/// One object with this level's named members removed or rewritten.
fn apply_to_object(
    tree: &DeleteTree,
    object: &Object,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let mut builder =
        ObjectBuilder::try_with_capacity(object.len()).map_err(|_| EngineRunError::allocation_failure())?;
    for entry in object {
        let branch = tree
            .entries
            .iter()
            .find(|branch| matches_key(&branch.component, entry.key()));
        let value = match branch {
            Some(branch) if branch.children.terminal => continue,
            Some(branch) => apply(&branch.children, try_clone(entry.value()), resources)?,
            None => try_clone(entry.value()),
        };
        builder
            .try_insert_last(entry.clone_key(), value)
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    // A component that names no member deletes nothing, but a component of the wrong KIND is still the type error
    // raised even when the object is empty.
    // The delete-path-miss cell fires per STRING component that named no member: `del(.a)` on a memberless object is a
    // silent no-op.
    for branch in &tree.entries {
        match branch.component.untagged() {
            Value::String(_) => {
                let matched = object.iter().any(|entry| matches_key(&branch.component, entry.key()));
                if !matched {
                    crate::error::mismatch::resolve_at(
                        resources,
                        crate::error::mismatch::MismatchCell::DeletePathMiss,
                        false,
                        (),
                    )?;
                }
            }
            _ => {
                return Err(delete_class(
                    owned_kind(&branch.component),
                    " field of object",
                    resources,
                ));
            }
        }
    }
    builder
        .try_finish()
        .map(Value::Object)
        .map_err(|_| EngineRunError::allocation_failure())
}

/// Whether one component names `key`.
fn matches_key(component: &Value, key: &str) -> bool {
    matches!(component.untagged(), Value::String(name) if &**name == key)
}

/// One array with this level's positions removed or rewritten — the reference's sorted-delete array law.
///
/// Non-terminal components rebuild their position or range in a WORKING array as the walk proceeds — each one
/// re-resolves against the array as it now stands, exactly as the reference mutates the object between groups — and
/// terminal keys are DEFERRED and resolved once, simultaneously, against the FINAL array (the reference's simultaneous
/// deletion). A terminal key therefore sees the shifts a rebuilt slice caused, while two terminal keys always resolve
/// against the same array: `delpaths([[{"start":0,"end":2},0],[1]])` over `[0,1,2,3]` rebuilds the slice to `[1]` and
/// then deletes position 1 of `[1,2,3]`, yielding `[1,3]`, while the sibling `[{"start":1,"end":3}]` slice deletes its
/// range of the same final array.
#[allow(
    clippy::too_many_lines,
    reason = "one linear walk: the coverage counter, the out-of-range keep, and the              per-element splice are one law read top to bottom"
)]
fn apply_to_array(tree: &DeleteTree, array: &Array, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let mut working = Vec::new();
    working
        .try_reserve_exact(array.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for value in array {
        working.push(try_clone(value));
    }
    let mut delkeys: Vec<Value> = Vec::new();
    for branch in &tree.entries {
        if branch.children.terminal {
            delkeys.push(try_clone(&branch.component));
            continue;
        }
        match branch.component.untagged() {
            Value::Number(number) => {
                let Some(position) = array_position(working.len(), number) else {
                    // The out-of-range half of the delete-path-miss cell:
                    // `delpaths([[5,0]])` over `[1,2]` names nothing.
                    crate::error::mismatch::resolve_at(
                        resources,
                        crate::error::mismatch::MismatchCell::DeletePathMiss,
                        false,
                        (),
                    )?;
                    continue;
                };
                let element = working
                    .get(position)
                    .ok_or_else(|| internal("a working array position vanished"))?;
                let rebuilt = apply(&branch.children, try_clone(element), resources)?;
                working[position] = rebuilt;
            }
            Value::Object(bounds) => {
                let (start, end) = slice_bounds(bounds, working.len(), resources)?;
                let segment = segment_of(&working, start, end, resources)?;
                let rebuilt = apply(&branch.children, segment, resources)?;
                let Value::Array(rebuilt) = rebuilt.untagged() else {
                    return Err(internal("a slice rebuild produced a non-array"));
                };
                let mut rebuilt_values = Vec::new();
                for value in rebuilt {
                    rebuilt_values.push(try_clone(value));
                }
                working.splice(start..end, rebuilt_values);
            }
            other => {
                return Err(delete_class(owned_kind(other), " element of array", resources));
            }
        }
    }
    // The deferred terminal keys, resolved simultaneously against the final working array (the reference's simultaneous
    // deletion): a number addresses its position, a slice object its range, and any other kind is the array-delete type
    // error. An out-of-range number key names no element and fires the delete-path-miss cell.
    let len = working.len();
    // One coverage counter over the final array, built once from the terminal keys: a number key marks its position, a
    // slice object its range, and any other kind is the array-delete type error. An out-of-range number key names no
    // element and fires the delete-path-miss cell. The emit walk below reads the running coverage, so deletion costs
    // one lookup per element instead of a scan over every key.
    let mut cut: Vec<i32> = Vec::new();
    cut.try_reserve_exact(len + 1)
        .map_err(|_| EngineRunError::allocation_failure())?;
    cut.resize(len + 1, 0);
    for key in &delkeys {
        match key.untagged() {
            Value::Number(number) => match array_position(len, number) {
                // A position past the end names no element to delete; the per-element scan this replaces never matched
                // one either, so it stays a silent no-op.
                Some(position) if position < len => {
                    cut[position] += 1;
                    cut[position + 1] -= 1;
                }
                Some(_) => {}
                None => {
                    crate::error::mismatch::resolve_at(
                        resources,
                        crate::error::mismatch::MismatchCell::DeletePathMiss,
                        false,
                        (),
                    )?;
                }
            },
            Value::Object(bounds) => {
                let (start, end) = slice_bounds(bounds, len, resources)?;
                cut[start] += 1;
                cut[end] -= 1;
            }
            other => {
                return Err(delete_class(owned_kind(other), " element of array", resources));
            }
        }
    }
    let mut out = Vec::new();
    out.try_reserve_exact(len)
        .map_err(|_| EngineRunError::allocation_failure())?;
    let mut covered = 0_i32;
    for (index, element) in working.iter().enumerate() {
        covered += cut[index];
        if covered == 0 {
            out.push(try_clone(element));
        }
    }
    Array::try_from_vec(out)
        .map(Value::Array)
        .map_err(|_| EngineRunError::allocation_failure())
}

/// The sub-array one non-terminal slice component addresses.
fn segment_of(
    working: &[Value],
    start: usize,
    end: usize,
    _resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(end - start)
        .map_err(|_| EngineRunError::allocation_failure())?;
    for index in start..end {
        match working.get(index) {
            Some(value) => values.push(try_clone(value)),
            None => return Err(internal("a slice segment ran past its array")),
        }
    }
    Array::try_from_vec(values)
        .map(Value::Array)
        .map_err(|_| EngineRunError::allocation_failure())
}

/// The reference's `Cannot delete fields from <kind>`: the container has no members at all.
fn cannot_delete_from(value: &Value, resources: &ResourceContext<'_>) -> EngineRunError {
    match message::cannot_delete_from_message(owned_kind(value)) {
        Ok(text) => raise(&text, resources),
        Err(error) => error,
    }
}

/// The per-container delete rejection (`Cannot delete <kind><suffix>`).
fn delete_class(component: jqf_data::ValueKind, suffix: &str, resources: &ResourceContext<'_>) -> EngineRunError {
    match message::cannot_delete_message(component, suffix) {
        Ok(text) => raise(&text, resources),
        Err(error) => error,
    }
}

/// A broken internal contract in the deletion walk.
fn internal(contract: &'static str) -> EngineRunError {
    EngineRunError::internal_contract(contract)
}

#[cfg(test)]
mod tests {
    use super::delete_paths;
    use alloc::format;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::fmt::Write as _;
    use jqf_data::{Array, Number, ObjectBuilder, ObjectKey, Value};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    /// One unlimited request ledger: every fixture is charged at construction.
    fn ledger() -> ResourceContext<'static> {
        let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let account = RequestAccount::try_new(limits).expect("test account");
        let work = WorkMeter::try_new_v1(1).expect("test work meter");
        ResourceContext::new(account, &CONTROL, work).expect("test ledger")
    }

    fn number(spelling: &str) -> Value {
        Value::Number(Number::try_json_literal(spelling).expect("literal"))
    }

    fn array(elements: Vec<Value>) -> Value {
        let _resources = ledger();
        Value::Array(Array::try_from_vec(elements).expect("array"))
    }

    /// `[1,2,3]` as a value.
    fn root() -> Value {
        array(vec![number("0"), number("1"), number("2"), number("3")])
    }

    fn slice(start: u64, end: u64) -> Value {
        let mut builder = ObjectBuilder::new();
        builder
            .try_insert_last(
                ObjectKey::try_from_str("start").expect("key"),
                number(alloc::format!("{start}").as_str()),
            )
            .expect("insert");
        builder
            .try_insert_last(
                ObjectKey::try_from_str("end").expect("key"),
                number(alloc::format!("{end}").as_str()),
            )
            .expect("insert");
        Value::Object(builder.try_finish().expect("object"))
    }

    fn paths(elements: Vec<Value>) -> Value {
        let _resources = ledger();
        Value::Array(Array::try_from_vec(elements).expect("paths"))
    }

    fn path(elements: Vec<Value>) -> Value {
        let _resources = ledger();
        Value::Array(Array::try_from_vec(elements).expect("path"))
    }

    fn render(value: &Value) -> String {
        match value {
            Value::Array(array) => {
                let mut out = String::from("[");
                for (index, element) in array.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    match element {
                        Value::Number(number) => {
                            let Some(integer) = number.to_integer() else {
                                out.push('?');
                                continue;
                            };
                            out.push_str(integer.as_str());
                        }
                        other => {
                            let _ = write!(out, "{other:?}");
                        }
                    }
                }
                out.push(']');
                out
            }
            other => format!("{other:?}"),
        }
    }

    /// A non-terminal slice rebuild shifts the positions the remaining paths address; the deferred terminal keys
    /// resolve against the final array.
    /// Each row pins the reference's own answer for the same program.
    #[test]
    fn slice_rebuilds_shift_subsequent_positions_like_jq() {
        let cases: Vec<(Vec<Value>, &str)> = vec![
            (
                vec![path(vec![slice(0, 2), number("0")]), path(vec![number("1")])],
                "[1,3]",
            ),
            (
                vec![path(vec![slice(0, 2), number("1")]), path(vec![number("0")])],
                "[2,3]",
            ),
            (
                vec![path(vec![slice(0, 2), number("0")]), path(vec![slice(1, 3)])],
                "[1]",
            ),
            (
                vec![
                    path(vec![slice(0, 2), number("1")]),
                    path(vec![slice(1, 3), number("0")]),
                    path(vec![number("2")]),
                ],
                "[0,3]",
            ),
        ];
        let resources = ledger();
        for (set, expected) in cases {
            let set = paths(set);
            let out = delete_paths(&root(), &set, &resources).expect("deletes");
            assert_eq!(render(&out), *expected, "set={set:?}");
        }
    }

    /// Terminal terminal components resolve simultaneously against the final array: two terminal slices delete their
    /// original ranges together, and a terminal index next to a terminal slice never re-resolves against the slice's
    /// result.
    #[test]
    fn terminal_keys_resolve_simultaneously() {
        let cases: Vec<(Vec<Value>, &str)> = vec![
            (vec![path(vec![slice(0, 2)]), path(vec![number("1")])], "[2,3]"),
            (vec![path(vec![slice(1, 3)]), path(vec![number("0")])], "[3]"),
            (vec![path(vec![slice(0, 2)]), path(vec![slice(1, 3)])], "[3]"),
            (
                vec![
                    path(vec![slice(0, 4), number("0")]),
                    path(vec![number("1")]),
                    path(vec![slice(1, 3)]),
                ],
                "[1]",
            ),
        ];
        let resources = ledger();
        for (set, expected) in cases {
            let set = paths(set);
            let out = delete_paths(&root(), &set, &resources).expect("deletes");
            assert_eq!(render(&out), *expected, "set={set:?}");
        }
    }
}
