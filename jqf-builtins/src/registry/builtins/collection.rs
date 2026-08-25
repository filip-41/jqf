//! Collection builtins: `map/1`.
//!
//! One job: own the `map` family and overload records. `map` is the first `Lowering`-kind builtin: `map(f)` expands at
//! lower time to the `[.[] | f]` graph, so it never reaches the executor as a `Call` node. The expansion is performed
//! by [`crate::compile`] (the arena builder), which this record's `Lowering` classification directs; ordinary fusion
//! then applies, so `map(f)` and `[.[] | f]` are literally the same plan.
//!
//! Negative space: no evaluator and no runtime dispatch — a `map` overload id never appears in a stored plan.

use super::id;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};

/// The `map` family record.
pub const FAMILIES: &[BuiltinFamilyRecord] = &[MAP_FAMILY];

/// The `map/1` overload record.
pub const OVERLOADS: &[BuiltinOverloadRecord] = &[MAP_OVERLOAD];

/// The collection execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Every entry carries its overload id so the const coverage walk in `registry::dispatch` can prove pairwise alignment.
#[derive(Clone, Copy, Debug)]
pub enum CollectionPayload {
    /// `map/1` — expands to `[.[] | f]`.
    Map,
}

pub const PAYLOADS: &[(u16, CollectionPayload)] = &[(id::MAP, CollectionPayload::Map)];

const MAP_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::MAP),
    canonical_name: "map",
    category: "collection",
    summary: "Collect `f` applied to each value of an array or object into a new \
              array.",
    detail: "Exactly `[.[] | f]`: iterate the input's values, run `f` over each, \
             and collect every output into one array. Iterating a non-iterable is \
             an error (from the expansion's `.[]`).",
};

const MAP_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::MAP),
    family: BuiltinFamilyId::new(id::MAP),
    canonical_name: "map",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Lowering,
    demand_transfer: DemandTransfer::ViaLowering,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "map(.id)",
            input: "[{\"id\":1},{\"id\":2}]",
            expected: "[1,2]\n",
        },
        BuiltinExample {
            program: "map(.)",
            input: "[1,2,3]",
            expected: "[1,2,3]\n",
        },
    ],
};
