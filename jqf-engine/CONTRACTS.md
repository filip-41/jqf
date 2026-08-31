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

- `try_compile_program(source, policy, options, resources)` is gate,
  preludes, parse, bind, lower, transform, analyze, and finish. The result is an
  opaque `CompiledProgram`.
- `CompileOptions` carries CLI bindings (`cli_vars`), lane flags
  (`split_exp` for the `$index` split-expression compile), and a source
  label (`source_label`, default `"<top-level>"`) for `$__loc__` and bind.
  A split compile sees no `--arg` family bindings.
- Stage 5 (`into_valid_syntax`) is the checked boundary: recoverable parse
  debris is rejected with the first parser diagnostic; an empty diagnostic list
  is an internal refusal, not a bound program.
- `--arg` / `--argjson` values lower to a literal at every reference
  site. A program binder always beats a CLI binding. Later CLI entries
  shadow earlier ones.
- `ParseRejection` and `UnsupportedConstruct` are owned by
  `jqf-builtins` and re-exported here.
- Arena allocation for the compiled program against the request account is
  fallible. A refused allocation is `EngineCompileError::Resource`.

## Analysis

- Arena facts (the codec-pushdown split, projection class, correlated
  scans, partial-sort table, range locate) live on the program. Nothing
  on the builtins side of the boundary reads them.
- Finish commits one shortcut (count, element, keys, type, has, any/all,
  min/max, range-locate, identity, or none — the graph). A new fast path
  is a new arm, not another optional field.
- Join, partial-sort, and count facts change how the executor walks, not
  what it publishes and not which codec requirement is lowered.
- Count, element, type, keys, has, any/all, and min/max demands are
  derived once at compile and consulted per record. A per-record
  re-derivation is a contract break.
- A document-side range row admits only non-negative bounds. A negative
  bound needs the container length and declines the row.

## Codec requirements

- `CompiledProgram::try_requirement` charges the access plan finish packed
  (Whole | Exact, prune, count/element/type/has/keys/minmax hints). `Err`
  aborts; it does not fall back to Whole. It invents no new requirement
  vocabulary and does not re-walk demands.
- Identity lowers as whole-document access. A static forward path
  lowers with its decoded steps. Identity never lowers as a forward
  requirement with an empty path.
- A nonempty static path on a document shortcut Exact-locates that
  path. Empty prefix is Whole. Decline of an Exact shortcut rebinds
  Whole when the graph still needs siblings the Exact node does not
  have.

## Execution

- `CompiledProgram::execute` tries the committed shortcut, then the
  residual graph. Decline is byte-identical to
  the graph. `host_io` is Echo | SpanCut | Run: identity echo and
  range-locate span cut stay host I/O. Count and element visit go
  through `Document::count_children_from` / `visit_elements_from`
  at the located node; keys, type, and has walk a view.
- Identity fallthrough is the identity residual. Host echo is
  retained-byte I/O (`HostIo::Echo`). Range-locate fallthrough is the
  graph floor. Host span cut is the codec session (`HostIo::SpanCut`).
  Neither is Exact-locate Element.
- JSON Exact count or element miss that cannot tell Exact from Whole
  returns `EngineRun::ReboundWhole`; the host decodes Whole. YAML/HTML
  native Exact also republish the selection as the product root, so
  that miss rebounds too. CompleteDocumentExact fallback names a child
  in the full graph: relocate to the document root and run Whole.
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
- `execute` assumes the codec already resolved the pushed-down prefix.
  `try_run_whole_value` runs the whole program when nothing did (`-n` /
  `-s` input is synthesized, never Exact-located).
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
