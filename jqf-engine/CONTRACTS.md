# jqf-engine Contracts

Invariants for this crate and for hosts. Type overview and examples live
in [README.md](README.md).

This crate does not parse or encode documents, open files, or publish
bytes. It compiles a program, derives analysis facts, and interprets
one residual graph.

It depends on `jqf-syntax`, `jqf-builtins`, `jqf-data`, `jqf-resource`,
`jqf-source`, and `jqf-codec-core`. The overload registry and builtin
evaluators live in `jqf-builtins` and are re-exported here.

## Compile

- `try_compile_program` is parse, lower, fuse, analyze, and charge. The
  result is an opaque `CompiledProgram`.
- `try_compile_program_for_edit` is the same compile under a named
  entry. Fact assignment lowers in every compile.
- `try_compile_program_split` is the only entry that binds `$index`. A
  split expression sees no `--arg` family bindings.
- `--arg` / `--argjson` values lower to a literal at every reference
  site. A program binder always beats a CLI binding. Later CLI entries
  shadow earlier ones.
- `ParseRejection` and `UnsupportedConstruct` are owned by
  `jqf-builtins` and re-exported here.
- Charging the compiled arena against the request account is fallible.
  A refused charge is `EngineCompileError::Resource`.

## Analysis

- Arena facts (the codec-pushdown split, projection class, correlated
  scans, partial-sort table, range locate) live on the program. Nothing
  on the builtins side of the boundary reads them.
- Join, partial-sort, and count facts change how the executor walks, not
  what it publishes and not which codec requirement is lowered.
- Count, element, type, and keys demands are derived once at compile and
  consulted per record. A per-record re-derivation is a contract break.
- A document-side range row admits only non-negative bounds. A negative
  bound needs the container length and declines the row.

## Codec requirements

- `CompiledProgram::try_requirement` lowers the derived split into a
  codec `AccessRequirement`. It invents no new requirement vocabulary.
- Identity lowers as whole-document access. A static forward path
  lowers with its decoded steps. Identity never lowers as a forward
  requirement with an empty path.

## Execution

- Located handles stay with their document until an explicit semantic
  construction produces an independently owned value.
- One value is in flight per frame. Nothing buffers a fan-out's
  children.
- A bare-`Stage` residual takes the single-slot fast path. A
  `Choice` / `FlatMap` residual engages the graph machine.
- Emission walks the live frame prefix from the top. The first consumer
  in range consumes the leaf. None in range is an ordered output item.
- Emitted-prefix-then-error is an ordered-stream property: item `k`'s
  failure surfaces on its own poll, after items `1..k-1`. No transition
  both emits and fails. An unsuppressed failure discards the frame
  stack.
- `try_run` assumes the codec already resolved the pushed-down prefix.
  `try_run_whole_value` runs the whole program when nothing did.
- `try_run_split` seeds `$index` with the item counter before the first
  poll. The seed is a no-op when the expression never references
  `$index`.
- Every advance charges the shared work meter and honors the same
  cooperative pauses the codec side uses.

## Errors

- `EngineRunError` and the arithmetic mismatch vocabulary live in
  `jqf-builtins` and are re-exported here.
- A machine failure (control, ledger, internal contract) is distinct
  from a raised program error. A label swallows `{"__jq": slot}` for
  its own slot; any other value is an ordinary error.
