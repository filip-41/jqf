//! The jqf DIFF extension family: the path-keyed semantic diff verb.
//!
//! `diff(old; new)` compares two documents structurally and emits ONE array of change records, ordered by path under
//! the engine's total order. The law is jqf's own (the reference has no diff; the path-keyed mode is the surveyed
//! precedent): at a path where both sides are objects it recurses over the union of their keys, at a path where both
//! sides are arrays it recurses over the union of their indices, and at a path where either side is a scalar or the
//! kinds differ it emits ONE `changed` record carrying both whole subtrees. A path present in one document only emits
//! an `added`/`removed` record; equal values emit nothing.
//!
//! Every record is `{"path": [...], "kind": "added"|"removed"|"changed"}` plus the kind's values (`old` for removed,
//! `new` for added, both for changed), and the record list is sorted by path so the output is deterministic for any
//! input order. Array diffs are POSITIONAL: index `i` of the old array compares to index `i` of the new one, so an
//! insertion shifts every later element's comparison (dyff's table alignment is a deliberate non-goal of v1).

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use jqf_data::{Array, Integer, ObjectBuilder, ObjectKey, Value};
use jqf_resource::ResourceContext;

use crate::error::EngineRunError;
use crate::registry::{
    BuiltinExample, BuiltinFamilyRecord, BuiltinOverloadRecord, DemandTransfer, Effects, ParameterKind,
    SemanticRevision,
};
use crate::semantics::order::{semantic_eq, total_cmp};

use super::id;

/// One change record, before it is materialized into an object.
struct Record {
    /// The path components as values: object keys as strings, array positions as numbers. Built once and used for both
    /// the sort and the record object.
    path: Value,
    kind: &'static str,
    old: Option<Value>,
    new: Option<Value>,
}

/// The family record.
pub const DIFF_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: crate::registry::BuiltinFamilyId::new(crate::registry::builtins::id::DIFF_FAMILY_ID),
    canonical_name: "diff",
    category: "jqf-extension",
    summary: "The path-keyed semantic diff.",
    detail: "Compare two documents structurally: added/removed/changed records, \
             ordered by path.",
};

const DIFF_EXAMPLES: &[BuiltinExample] = &[
    BuiltinExample {
        program: "diff({\"a\":1}; {\"a\":2,\"b\":3})",
        input: "null",
        expected: concat!(
            "[{\"path\":[\"a\"],\"kind\":\"changed\",\"old\":1,\"new\":2},",
            "{\"path\":[\"b\"],\"kind\":\"added\",\"new\":3}]\n"
        ),
    },
    BuiltinExample {
        program: "diff([1,2]; [2,3])",
        input: "null",
        expected: concat!(
            "[{\"path\":[0],\"kind\":\"changed\",\"old\":1,\"new\":2},",
            "{\"path\":[1],\"kind\":\"changed\",\"old\":2,\"new\":3}]\n"
        ),
    },
    BuiltinExample {
        program: "diff({\"a\":{\"x\":1}}; {\"a\":{\"y\":2}})",
        input: "null",
        expected: concat!(
            "[{\"path\":[\"a\",\"x\"],\"kind\":\"removed\",\"old\":1},",
            "{\"path\":[\"a\",\"y\"],\"kind\":\"added\",\"new\":2}]\n"
        ),
    },
];

/// The `diff/2` overload record: two filter arguments (the old and the new document), evaluated over the input by the
/// ordinary argument law.
pub const DIFF_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: crate::registry::BuiltinOverloadId::new(crate::registry::builtins::id::DIFF_2),
    family: crate::registry::BuiltinFamilyId::new(crate::registry::builtins::id::DIFF_FAMILY_ID),
    canonical_name: "diff",
    arity: 2,
    parameters: &[ParameterKind::Filter, ParameterKind::Filter],
    execution: crate::registry::BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: DIFF_EXAMPLES,
};

/// The overload and family slices the registry aggregates.
pub const FAMILIES: &[BuiltinFamilyRecord] = &[DIFF_FAMILY];
pub const OVERLOADS: &[BuiltinOverloadRecord] = &[DIFF_OVERLOAD];

/// The diff execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Every entry carries its overload id so the const coverage walk in `registry::dispatch` can prove pairwise alignment.
#[derive(Clone, Copy, Debug)]
pub enum DiffPayload {
    /// `diff/2` — the path-keyed semantic comparison.
    Diff,
}

pub const PAYLOADS: &[(u16, DiffPayload)] = &[(id::DIFF_2, DiffPayload::Diff)];

/// One step of the explicit worklist the walk drives.
struct Step {
    old: Value,
    new: Value,
    path: Vec<Value>,
}

/// How the diff walk's array arm compares two arrays.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArrayCompare {
    /// The ordinary positional law: index `i` of the old array compares to index `i` of the new one.
    Positional,
    /// The `schema_diff` law: at a path whose last component is a set keyword (`required`, `enum`), two arrays compare
    /// as sets — equal sets emit nothing, differing sets emit one `changed` record carrying both arrays. Everywhere
    /// else the positional law stands.
    SetKeywords,
}

/// Runs the path-keyed semantic diff over two owned documents.
///
/// The walk is ITERATIVE (an explicit worklist, not recursion), so its stack cost is independent of the documents'
/// nesting. The record list is sorted by path under the engine's total order, which is what makes the output
/// deterministic.
pub fn diff_law(left: Value, right: Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    diff_law_with(left, right, ArrayCompare::Positional, resources)
}

/// The array-comparison-mode form of [`diff_law`], shared by `diff/2` and `schema_diff/2`.
#[expect(
    clippy::too_many_lines,
    reason = "one law with two container arms, the set-keyword arm, and the record materialization; \
              splitting it would separate the walk from its output law"
)]
pub fn diff_law_with(
    left: Value,
    right: Value,
    array_compare: ArrayCompare,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let mut records: Vec<Record> = Vec::new();
    let mut work: Vec<Step> = vec![Step {
        old: left,
        new: right,
        path: Vec::new(),
    }];
    while let Some(Step { old, new, path }) = work.pop() {
        if semantic_eq(&old, &new).map_err(|_| too_deep(resources))? {
            continue;
        }
        let old_ref = old.untagged();
        let new_ref = new.untagged();
        match (old_ref, new_ref) {
            (Value::Object(old_object), Value::Object(new_object)) => {
                // The union of both sides' keys, walked in the total order's restriction to strings (plain
                // lexicographic) so the walk is deterministic; the record sort makes the OUTPUT order independent of it
                // either way.
                let mut keys: Vec<String> = Vec::new();
                // Both sides go in unconditionally and dedup rides the sort: a
                // per-entry membership scan is quadratic in the key count.
                for side in [old_object, new_object] {
                    for entry in side {
                        keys.push(String::from(entry.key()));
                    }
                }
                keys.sort_unstable();
                keys.dedup();
                for key in keys {
                    let mut child = clone_path(&path)?;
                    let component = Value::try_string(&key).map_err(|_| EngineRunError::allocation_failure())?;
                    child.push(component);
                    match (old_object.get(&key), new_object.get(&key)) {
                        (Some(old_child), Some(new_child)) => work.push(Step {
                            old: old_child.clone(),
                            new: new_child.clone(),
                            path: child,
                        }),
                        (Some(removed), None) => records.push(Record {
                            path: array(child, resources)?,
                            kind: "removed",
                            old: Some(removed.clone()),
                            new: None,
                        }),
                        (None, Some(added)) => records.push(Record {
                            path: array(child, resources)?,
                            kind: "added",
                            old: None,
                            new: Some(added.clone()),
                        }),
                        (None, None) => {}
                    }
                }
            }
            (Value::Array(old_array), Value::Array(new_array)) => {
                // The schema_diff set-keyword rule: at a path whose last component is `required` or `enum`, the two
                // arrays compare as SETS — order noise emits nothing, a differing set emits ONE changed record
                // carrying both arrays. Everywhere else the positional law stands.
                let set_keyword = matches!(array_compare, ArrayCompare::SetKeywords)
                    && path.last().is_some_and(|component| {
                        matches!(
                            component.untagged(),
                            Value::String(text) if text.as_str() == "required" || text.as_str() == "enum"
                        )
                    });
                if set_keyword {
                    if !same_array_set(old_array, new_array, resources)? {
                        records.push(Record {
                            path: array(path, resources)?,
                            kind: "changed",
                            old: Some(old),
                            new: Some(new),
                        });
                    }
                    continue;
                }
                // The union of both sides' positions, walked in index order.
                let width = old_array.len().max(new_array.len());
                for position in 0..width {
                    let mut child = clone_path(&path)?;
                    child.push(Value::Number(jqf_data::Number::integer(Integer::from_i64(
                        i64::try_from(position).unwrap_or(i64::MAX),
                    ))));
                    match (old_array.get(position), new_array.get(position)) {
                        (Some(old_child), Some(new_child)) => work.push(Step {
                            old: old_child.clone(),
                            new: new_child.clone(),
                            path: child,
                        }),
                        (Some(removed), None) => records.push(Record {
                            path: array(child, resources)?,
                            kind: "removed",
                            old: Some(removed.clone()),
                            new: None,
                        }),
                        (None, Some(added)) => records.push(Record {
                            path: array(child, resources)?,
                            kind: "added",
                            old: None,
                            new: Some(added.clone()),
                        }),
                        (None, None) => {}
                    }
                }
            }
            // A scalar on either side, or a kind change: ONE changed record carrying both whole subtrees.
            _ => records.push(Record {
                path: array(path, resources)?,
                kind: "changed",
                old: Some(old),
                new: Some(new),
            }),
        }
    }
    // The record list, ordered by path under the total order. The comparison guard is unreachable in practice — the
    // paths are bounded by the documents' own nesting, which the construction cap already bounded — and sort order is
    // a determinism law, not a correctness one, so a pathological depth degrades to an arbitrary but STABLE order
    // rather than failing the diff.
    records.sort_by(|left, right| total_cmp(&left.path, &right.path).unwrap_or(core::cmp::Ordering::Equal));
    let mut output = Vec::new();
    output
        .try_reserve_exact(records.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for record in records {
        let mut builder = ObjectBuilder::try_with_capacity(4).map_err(|_| EngineRunError::allocation_failure())?;
        builder
            .try_insert_last(
                ObjectKey::try_from_str("path").map_err(|_| EngineRunError::allocation_failure())?,
                record.path,
            )
            .map_err(|_| EngineRunError::allocation_failure())?;
        builder
            .try_insert_last(
                ObjectKey::try_from_str("kind").map_err(|_| EngineRunError::allocation_failure())?,
                Value::try_string(record.kind).map_err(|_| EngineRunError::allocation_failure())?,
            )
            .map_err(|_| EngineRunError::allocation_failure())?;
        if let Some(old) = record.old {
            builder
                .try_insert_last(
                    ObjectKey::try_from_str("old").map_err(|_| EngineRunError::allocation_failure())?,
                    old,
                )
                .map_err(|_| EngineRunError::allocation_failure())?;
        }
        if let Some(new) = record.new {
            builder
                .try_insert_last(
                    ObjectKey::try_from_str("new").map_err(|_| EngineRunError::allocation_failure())?,
                    new,
                )
                .map_err(|_| EngineRunError::allocation_failure())?;
        }
        let object = builder.try_finish().map_err(|_| EngineRunError::allocation_failure())?;
        output.push(Value::Object(object));
    }
    let array = Array::try_from_vec(output).map_err(|_| EngineRunError::allocation_failure())?;
    Ok(Value::Array(array))
}

/// Clones one path's components into a fresh owned vector (a `Value` is not `Clone` by design; every component copies
/// through `try_clone`).
fn clone_path(path: &[Value]) -> Result<Vec<Value>, EngineRunError> {
    let mut cloned = Vec::new();
    cloned
        .try_reserve_exact(path.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for component in path {
        cloned.push(component.clone());
    }
    Ok(cloned)
}

/// Wraps one path's components into an owned array value.
fn array(components: Vec<Value>, _resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    Array::try_from_vec(components)
        .map(Value::Array)
        .map_err(|_| EngineRunError::allocation_failure())
}

/// Whether two arrays hold the same set of elements — the `schema_diff`
/// `required`/`enum` comparison. Set equality, not multiset:
/// schema keyword arrays hold unique members by definition, so duplicates cannot create a false difference. `O(n^2)`
/// semantic equality; schema keyword arrays are small and this is a drift verb, not a data path.
fn same_array_set(old: &Array, new: &Array, resources: &ResourceContext<'_>) -> Result<bool, EngineRunError> {
    'outer: for old_item in old {
        for new_item in new {
            if semantic_eq(old_item, new_item).map_err(|_| too_deep(resources))? {
                continue 'outer;
            }
        }
        return Ok(false);
    }
    for new_item in new {
        let mut found = false;
        for old_item in old {
            if semantic_eq(old_item, new_item).map_err(|_| too_deep(resources))? {
                found = true;
                break;
            }
        }
        if !found {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Maps the equality guard's depth refusal onto the engine's raise — the same `Equality check too deep` the `==` law
/// raises for the same inputs, so a diff of two beyond-cap documents fails exactly as comparing them would.
fn too_deep(resources: &ResourceContext<'_>) -> EngineRunError {
    crate::semantics::depth::equality_error(resources)
}
