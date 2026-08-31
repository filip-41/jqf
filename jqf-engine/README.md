# jqf-engine

Compiles, analyzes, and executes jqf programs.

This crate is `no_std` and uses `alloc`. It depends on `jqf-syntax` for
the program tree, `jqf-builtins` for the registry and evaluators,
`jqf-data` for values and documents, `jqf-resource` for the work budget
and cancel, `jqf-source` for spans, and `jqf-codec-core` for access
requirements. It does not parse documents, encode output, or open files.

What it has:

- `try_compile_program` — gate, preludes, parse, bind, lower, transform,
  analyze, and finish
- `CompileOptions` — CLI bindings (`cli_vars`), compile-lane flags
  (`split_exp`), and source label (`source_label`, default `"<top-level>"`)
- `CompiledProgram` — the opaque arena, plus `try_requirement` and `try_run`
- `CodecRequirementPolicy` — validation and diagnostic axes the lowering keeps
- `EngineRun` / `EngineRunStream` / `RunPoll` — one interpreted result stream
- `ExplainPlan` / `PlanRecord` — the explain surface
- `ProjectionClass` / `BoundaryConsumer` — analysis facts the drives consult
- the builtin registry and error vocabulary, re-exported from `jqf-builtins`

## Compile

`try_compile_program` is the public compile entry: gate, preludes, parse,
bind, lower, transform, analyze, and finish. It refuses source the
syntax crate refuses, constructs outside the landed subset, and a
ledger that cannot charge the arena.

```rust
use jqf_codec_core::{DiagnosticPolicy, ValidationMode};
use jqf_engine::{CodecRequirementPolicy, CompileOptions, try_compile_program};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

static CONTROL: ContinueControl = ContinueControl;
let limits = ResourceLimits::new(u64::MAX, 4096, 1 << 20, u64::MAX, 100);
let resources = ResourceContext::new(
    RequestAccount::try_new(limits).unwrap(),
    &CONTROL,
    WorkMeter::try_new_v1(4096).unwrap(),
)
.unwrap();

let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
let compiled = try_compile_program(". + 1", policy, CompileOptions::new(), &resources).unwrap();
assert!(!compiled.uses_inputs_cursor());
let _ = compiled.try_requirement(&resources).unwrap();
```

## Run

`CompiledProgram::try_run` drives the residual graph after a codec has
resolved the pushed-down prefix. `try_run_whole_value` runs the whole
program when nothing resolved that prefix (`-n`, `-s`, and their record
siblings). One value is in flight per frame; an unsuppressed failure
discards the frame stack.

## Contracts

See [`CONTRACTS.md`](CONTRACTS.md) for compile, analysis, and execution
invariants.
