//! Path builtins: `path/1`, `paths/0`, `paths/1`, `getpath/1`, `setpath/2`, `delpaths/1`, `del/1`.
//!
//! One job: own the family and overload records for the vocabulary that talks about LOCATIONS rather than values. Five
//! of the seven are evaluators the executor drives natively — `path` runs its argument in path mode, `paths` runs the
//! descend walk in path mode with the root excluded, and `getpath`/`setpath`/`delpaths` are pure value laws over a path
//! array. The other two are `Lowering`s expanded into the reference definitions, so their edge cases are inherited
//! rather than reimplemented:
//!
//! ```text
//! def paths(f):   path(.. | select(f)) | select(length > 0);
//! def del(f):     delpaths([path(f)]);
//! ```
//!
//! Every one of the seven declares [`DemandTransfer::Subtree`] or [`DemandTransfer::ViaLowering`]. There is no
//! shallower honest answer: a path expression's argument may navigate arbitrarily deep, `..` visits the whole document
//! by construction, and a `setpath` rebuilds every container on the way down. `paths` in particular is the one builtin
//! whose OUTPUT is a function of the input's complete structure.
//!
//! Negative space: no assignment operator is registered here — `|=`, `=`, and the arithmetic-update family are the
//! next vertical, and they consume this machinery rather than extending it.

use super::id;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};

/// The path family records.
pub const FAMILIES: &[BuiltinFamilyRecord] = &[
    PATH_FAMILY,
    PATHS_FAMILY,
    GETPATH_FAMILY,
    SETPATH_FAMILY,
    DELPATHS_FAMILY,
    DEL_FAMILY,
];

/// The path overload records.
pub const OVERLOADS: &[BuiltinOverloadRecord] = &[
    PATH_OVERLOAD,
    PATHS_ZERO_OVERLOAD,
    PATHS_ONE_OVERLOAD,
    GETPATH_OVERLOAD,
    SETPATH_OVERLOAD,
    DELPATHS_OVERLOAD,
    DEL_OVERLOAD,
];

/// The path execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// The family spans evaluators and lowerings, so it names its own payload enum; `registry::dispatch` wraps it at table
/// build time. Every entry carries its overload id so the const coverage walk there can prove pairwise alignment.
#[derive(Clone, Copy, Debug)]
pub enum PathsPayload {
    /// `path/1` — the path-mode evaluation.
    Path,
    /// `paths/0` — the root-excluding path walk.
    Paths,
    /// `paths/1` — expands to `path(.. | select(f)) | select(length > 0)`.
    PathsFiltered,
    /// `getpath/1` — the path read.
    GetPath,
    /// `setpath/2` — the path write.
    SetPath,
    /// `delpaths/1` — the simultaneous multi-path deletion.
    DelPaths,
    /// `del/1` — expands to `delpaths([path(f)])`.
    Del,
}

pub const PAYLOADS: &[(u16, PathsPayload)] = &[
    (id::PATH, PathsPayload::Path),
    (id::PATHS_ZERO, PathsPayload::Paths),
    (id::PATHS_ONE, PathsPayload::PathsFiltered),
    (id::GETPATH, PathsPayload::GetPath),
    (id::SETPATH, PathsPayload::SetPath),
    (id::DELPATHS, PathsPayload::DelPaths),
    (id::DEL, PathsPayload::Del),
];

const PATH_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::PATH),
    canonical_name: "path",
    category: "path",
    summary: "Emit the PATH each output of `f` was reached by, instead of the \
              value.",
    detail: "`f` is evaluated in path mode: every navigation extends a tracked \
             path, and every output publishes that path as an array of string \
             keys, number indices, and `{\"start\":s,\"end\":e}` slice objects. An \
             output that is not the value at the tracked path is the error \
             `Invalid path expression with result <v>`; a navigation over such a \
             value is `Invalid path expression near attempt to access element \
             <k> of <v>`.",
};

const PATH_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::PATH),
    family: BuiltinFamilyId::new(id::PATH),
    canonical_name: "path",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "path(.a.b)",
            input: "{\"a\":{\"b\":1}}",
            expected: "[\"a\",\"b\"]\n",
        },
        BuiltinExample {
            program: "[path(.[])]",
            input: "[1,2]",
            expected: "[[0],[1]]\n",
        },
    ],
};

const PATHS_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::PATHS),
    canonical_name: "paths",
    category: "path",
    summary: "Every path in the input except the root, optionally filtered by \
              the value each path addresses.",
    detail: "`paths` is `path(..) | select(length > 0)` evaluated natively: \
             the descend walk in path mode, publishing every non-root register. \
             `paths(f)` is the lowering `path(.. | select(f)) | select(length > 0)`. \
             The root is excluded by the length test, which is why a scalar input \
             produces nothing.",
};

const PATHS_ZERO_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::PATHS_ZERO),
    family: BuiltinFamilyId::new(id::PATHS),
    canonical_name: "paths",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[paths]",
            input: "{\"a\":[1]}",
            expected: "[[\"a\"],[\"a\",0]]\n",
        },
        BuiltinExample {
            program: "[paths]",
            input: "1",
            expected: "[]\n",
        },
    ],
};

const PATHS_ONE_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::PATHS_ONE),
    family: BuiltinFamilyId::new(id::PATHS),
    canonical_name: "paths",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Lowering,
    demand_transfer: DemandTransfer::ViaLowering,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[paths(type == \"number\")]",
            input: "{\"a\":1,\"b\":[2]}",
            expected: "[[\"a\"],[\"b\",0]]\n",
        },
        BuiltinExample {
            program: "[paths(false)]",
            input: "{\"a\":1}",
            expected: "[]\n",
        },
    ],
};

const GETPATH_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::GETPATH),
    canonical_name: "getpath",
    category: "path",
    summary: "The value at one path, or `null` when the path addresses nothing.",
    detail: "A missing member reads as `null` rather than erroring, and so does \
             every member below a `null`. `getpath` is itself a path expression: \
             inside `path(...)` it EXTENDS the tracked path by its argument's \
             components.",
};

const GETPATH_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::GETPATH),
    family: BuiltinFamilyId::new(id::GETPATH),
    canonical_name: "getpath",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "getpath([\"a\",\"b\"])",
            input: "{\"a\":{\"b\":1}}",
            expected: "1\n",
        },
        BuiltinExample {
            program: "getpath([\"x\",\"y\"])",
            input: "{\"a\":1}",
            expected: "null\n",
        },
    ],
};

const SETPATH_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::SETPATH),
    canonical_name: "setpath",
    category: "path",
    summary: "The input with one path set to a value, creating missing \
              containers.",
    detail: "A `null` on the way down becomes the container the next component \
             names — an object for a string key, an array for a number or slice \
             — and an array index past the end is null-padded. A negative index \
             that wraps below zero is `Out of bounds negative array index`.",
};

const SETPATH_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::SETPATH),
    family: BuiltinFamilyId::new(id::SETPATH),
    canonical_name: "setpath",
    arity: 2,
    parameters: &[ParameterKind::Filter, ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "setpath([\"a\"]; 9)",
            input: "{\"a\":1}",
            expected: "{\"a\":9}\n",
        },
        BuiltinExample {
            program: "setpath([\"a\",0]; 1)",
            input: "null",
            expected: "{\"a\":[1]}\n",
        },
    ],
};

const DELPATHS_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::DELPATHS),
    canonical_name: "delpaths",
    category: "path",
    summary: "The input with every listed path removed, simultaneously.",
    detail: "The paths are applied as one edit, against the input's ORIGINAL \
             positions, so overlapping deletions neither compound nor shift each \
             other. A path that is a prefix of another swallows it.",
};

const DELPATHS_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::DELPATHS),
    family: BuiltinFamilyId::new(id::DELPATHS),
    canonical_name: "delpaths",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "delpaths([[\"a\"]])",
            input: "{\"a\":1,\"b\":2}",
            expected: "{\"b\":2}\n",
        },
        BuiltinExample {
            program: "delpaths([[0],[0,0]])",
            input: "[1,2,3]",
            expected: "[2,3]\n",
        },
    ],
};

const DEL_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::DEL),
    canonical_name: "del",
    category: "path",
    summary: "The input with everything `f` names removed.",
    detail: "Exactly `delpaths([path(f)])`: `f` runs in path mode, and every \
             path it produces is deleted in one simultaneous edit — which is why \
             `del(.[0], .[1])` removes both original positions.",
};

const DEL_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::DEL),
    family: BuiltinFamilyId::new(id::DEL),
    canonical_name: "del",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Lowering,
    demand_transfer: DemandTransfer::ViaLowering,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "del(.a)",
            input: "{\"a\":1,\"b\":2}",
            expected: "{\"b\":2}\n",
        },
        BuiltinExample {
            program: "del(.[1])",
            input: "[1,2,3]",
            expected: "[1,3]\n",
        },
        // A MULTI-VALUED path argument names every path to delete, not just one.
        BuiltinExample {
            program: "del(.a, .b)",
            input: "{\"a\":1,\"b\":2,\"c\":3}",
            expected: "{\"c\":3}\n",
        },
    ],
};
