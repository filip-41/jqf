//! The jqf user-declared reusable index: `declare_index/2` (a jqf extension — the reference has no index concept at
//! all).
//!
//! `declare_index(CONTAINER; KEY)` is a TRANSPARENT ACCELERATION DECLARATION: it builds a sorted keyed multimap over
//! the located container at CONTAINER, keyed on each child's KEY path, and the call's OUTPUT is the input, unchanged
//! — declaring an index may change SPEED, never BYTES. Both arguments are static paths spelled as filters (`.users`,
//! `.id`) and are PATTERN-MATCHED, never evaluated, following the `path(f)` precedent.
//!
//! The declaration reuses the transient `JoinIndex`/`KeyedCollect` machinery (`crate::exec::build_join_index`) and the
//! sorted-`total_cmp` law, so the index's equality cannot drift from `==` (sound by construction). A declared index
//! turns every LATER recognized scan over the same (container node, key path) — the correlated-scan and anti-join
//! rows of `crate::analysis::join` — from a full scan into an index probe, with the user owning the one-scan build
//! cost.
//!
//! The NaN-key decline is inherited: `build_join_index` refuses an index whose keys carry a NaN, so a NaN-bearing
//! container simply declines to the naive scan.
//!
//! ## CLI-usable, not SDK-only
//!
//! The declaration and its probes live in ONE program (`declare_index(.users; .id) | [.users[] | select(.id == 42)]`),
//! so an ordinary `jqf` invocation demonstrates the whole lifecycle — the machine's user-index store persists across
//! the comma/pipe arms of one run.

use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};

use super::id;

const fn example(program: &'static str, input: &'static str, expected: &'static str) -> BuiltinExample {
    BuiltinExample {
        program,
        input,
        expected,
    }
}

const fn family(id: u16, name: &'static str, summary: &'static str, detail: &'static str) -> BuiltinFamilyRecord {
    BuiltinFamilyRecord {
        id: BuiltinFamilyId::new(id),
        canonical_name: name,
        category: "jqf-extension",
        summary,
        detail,
    }
}

/// The `declare_index/2` family record.
pub const INDEX_FAMILY: BuiltinFamilyRecord = family(
    id::DECLARE_INDEX_FAMILY_ID,
    "declare_index",
    "Declare a reusable index on a container field.",
    "`declare_index(CONTAINER; KEY)` builds a sorted keyed index over the located \
     container at CONTAINER, keyed on each child's KEY path, and passes the \
     input through unchanged. Every later scan over the same (container, key \
     path) — `.users[] | select(.id == …)`, `all(.users[]; .id != …)` — probes \
     the index instead of scanning, byte-identically (the sorted total_cmp \
     law). Building the index costs one full scan; the user owns that cost. \
     Both arguments are static paths spelled as filters; the declaration is \
     transparent — any shape it cannot serve declines to the naive scan \
     without changing the output.",
);

/// The `declare_index/2` overload record.
pub const INDEX_DECLARE: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::DECLARE_INDEX),
    family: BuiltinFamilyId::new(id::DECLARE_INDEX_FAMILY_ID),
    canonical_name: "declare_index",
    arity: 2,
    parameters: &[ParameterKind::Filter, ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        example(
            "declare_index(.users; .id)",
            r#"{"users":[{"id":1,"name":"Ada"},{"id":2,"name":"Bob"}]}"#,
            concat!(r#"{"users":[{"id":1,"name":"Ada"},{"id":2,"name":"Bob"}]}"#, "\n"),
        ),
        example(
            "declare_index(.users; .id) | [.users[] | select(.id == 2)]",
            r#"{"users":[{"id":1,"name":"Ada"},{"id":2,"name":"Bob"}]}"#,
            concat!(r#"[{"id":2,"name":"Bob"}]"#, "\n"),
        ),
        example(
            "declare_index(.users; .id) | [.users[] | select(.id == 9)]",
            r#"{"users":[{"id":1,"name":"Ada"},{"id":2,"name":"Bob"}]}"#,
            concat!(r#"[]"#, "\n"),
        ),
    ],
};

/// The `index` family slices, in split order.
pub const FAMILIES: &[BuiltinFamilyRecord] = &[INDEX_FAMILY];
pub const OVERLOADS: &[BuiltinOverloadRecord] = &[INDEX_DECLARE];

/// The index execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Every entry carries its overload id so the const coverage walk in `registry::dispatch` can prove pairwise alignment.
#[derive(Clone, Copy, Debug)]
pub enum IndexPayload {
    /// `declare_index/2` — the transparent acceleration declaration.
    Declare,
}

pub const PAYLOADS: &[(u16, IndexPayload)] = &[(id::DECLARE_INDEX, IndexPayload::Declare)];
