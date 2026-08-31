# jqf-sdk

Composes codecs and the engine into one driven request.

This crate selects registrations, preserves engine item boundaries, and
owns output publication accounting. Format grammar, program semantics,
host I/O, and facade framing policy stay outside it.

An embedder needs this crate plus one codec crate. Compile entry points,
the compiled program, the resource context, and the source vocabulary
are re-exported here.

What it has:

- `execute` — the one routing entry
- `Request` / `Input` / `Outcome` / `Report` / `Failure` — the request
- `CodecCatalog` — the registrations one request may select
- `ItemSink` / `FacadeFraming` — how published bytes leave
- `PipelinePolicy` / `PipelineFailure` — decode, encode, and drive errors
- `try_compile_program` / `CompileOptions` / `CompiledProgram` / `CodecRequirementPolicy` —
  re-exported from `jqf-engine`
- `request_stack_bytes` — the sized-thread helper compile and execute need

## Compile and execute

`execute` picks the drive from the request. The named drives are
crate-private.

```rust
use jqf_codec_json::registration;
use jqf_sdk::{
    CodecCatalog, CodecRequirementPolicy, CompileOptions, ContinueControl, DiagnosticPolicy, RequestAccount,
    ResourceContext, ResourceLimits, ValidationMode, WorkMeter, try_compile_program,
};

static CONTROL: ContinueControl = ContinueControl;
let limits = ResourceLimits::new(u64::MAX, 4096, 1 << 20, u64::MAX, 100);
let resources = ResourceContext::new(
    RequestAccount::try_new(limits).unwrap(),
    &CONTROL,
    WorkMeter::try_new_v1(64).unwrap(),
)
.unwrap();

let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
let compiled = try_compile_program(".catalog[].name", policy, CompileOptions::new(), &resources).unwrap();
assert!(!compiled.uses_inputs_cursor());

let json = registration().unwrap();
let _catalog = CodecCatalog::new(&[&json]);
```

A full compile-and-publish walk lives in
[`examples/compile_execute.rs`](examples/compile_execute.rs).

## Request thread

`try_compile_program` and `execute` recurse on the call stack. The
documented nesting refusal needs a large stack (default 256 MiB).
`Request` and `ResourceContext` are `!Send`, so spawn the sized thread
first, construct the request on that thread, and join only the `Result`.

## Contracts

See [`CONTRACTS.md`](CONTRACTS.md) for request, routing, and publication
invariants.
