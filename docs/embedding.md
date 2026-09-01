# Embedding jqf

Rust embeds `jqf-sdk` directly; C and everything with a C FFI embeds
`jqf-sdk-ffi`; Python and the browser have bindings built on those. Resource
governance applies to embedded calls the same way it applies to the binary —
every execution runs under an accounted ledger you configure.

## Rust: `jqf-sdk`

`jqf_sdk::execute` is the one routing entry — the route-named drives behind it
are crate-private.

```rust
pub fn execute<S: ItemSink>(request: Request<'_, '_, '_>, sink: &mut S)
    -> Result<Outcome, Failure>
```

A request names a compiled program, an input (`Whole` bytes, a
`Streaming` reader, or `Records`), the access requirement the program
lowers, a codec catalog, formats and dialects, policy, and a
`ResourceContext`. The sketch (from
`jqf-sdk/examples/compile_execute.rs`):

```rust
let registration = jqf_codec_json::registration()?;
let catalog = CodecCatalog::new(&[&registration]);

let mut resources = ResourceContext::new(
    RequestAccount::try_new(ResourceLimits::new(
        u64::MAX, u64::MAX, 64 << 20, 0, 128))?,
    &ContinueControl,
    WorkMeter::try_new_v1(64)?,
)?;

let program = try_compile_program(
    ".catalog[].name",
    policy,
    CompileOptions::new(),
    &resources,
)?;
let requirement = program.try_requirement(&resources)?;
let outcome = execute(
    Request::new(&program, Input::Whole(input))
        .with_catalog(catalog)
        .with_format(format(), dialect())
        .with_output_format(format(), dialect())
        .with_requirement(&requirement)
        .with_resources(&mut resources),
    &mut sink,
)?;
```

### Compile

`try_compile_program` is the same entry the CLI uses. It takes the program
source, a requirement policy, `CompileOptions`, and the request ledger.
`CompileOptions::new()` is the ordinary lane; the split-expression lane
(`$index` for `--split-exp` / `--split-exp-file`) is
`CompileOptions::split_exp()` and sees no `--arg` family bindings. What those
options *do* is [Engine compiler](engine-compiler.md). What *execute* does with
the compiled job is [Engine executor](engine-exec.md).

**Codecs are dependency edges.** Registration is one line per codec —
`jqf_codec_yaml::registration()`, `jqf_codec_toml::registration_1_0()`,
`jqf_codec_html::registration_fragment()`, … — and a build understands exactly
the formats it registered. The CLI itself is just a caller that registers all
twenty-three.

The one thing `jqf-sdk` gates by feature is the builtin extension families
(`ext-hash`, `ext-schema`, `ext-jsonpath`, `ext-net`, `ext-fuzzy`, `ext-redact`)
— all six on by default, droppable with `--no-default-features`. See
[Builtins](builtins.md).

`Request` borrows the program, so a resident service reuses one compilation
across calls. Requests are `!Send` — one request runs on one thread; parallelism
inside a request belongs to [the runtime](parallelism.md).

## C: `jqf-sdk-ffi`

A C ABI (`JQF_ABI_VERSION` 2, checked at load) with strict-JSON values at the
boundary. The surface is small and ownership is one-sided: the caller owns every
buffer, no engine pointer escapes.

| Group         | Functions                                                                              |
| ------------- | -------------------------------------------------------------------------------------- |
| lifecycle     | `jqf_new`, `jqf_new_limited`, `jqf_free`                                               |
| compile once  | `jqf_compile`, `jqf_compile_args`, `jqf_program_free`                                  |
| run           | `jqf_run`, `jqf_run_compiled`, `jqf_run_sequence`, `jqf_run_sequence_streaming`        |
| resident feed | `jqf_feed_open`, `jqf_feed_push`, `jqf_feed_poll`, `jqf_feed_finish`, `jqf_feed_close` |
| diagnostics   | `jqf_diag_*`, `jqf_run_errors_*`                                                       |

The **resident NDJSON feed** is [serve mode](serve.md) as a library: open a feed
against a compiled program (strict or recovering profile), `push` bytes as they
arrive, `poll` completed outputs into your buffer, `finish` to deliver the held
tail. The same held-tail law as `--follow` — a partial record is never guessed
at.

## Python

The `bindings/python` package wraps the FFI with ctypes — no native extension to
build; the wheel bundles the shared library.

```python
import jqf
assert jqf.run(".n + 1", b'{"n":1}').output.strip() == b"2"

with jqf.Session() as s:
    prog = s.compile(".n + 1")
    feed = s.open_feed(prog, "recovering")
```

`run` / `run_many` are the one-shot calls; `Session` + `Program` amortize
compilation; `Feed` is the resident record stream.

## WebAssembly

`bindings/wasm` builds the `jqf_wasm` crate (its own ABI, version 1): `jqf_run`
executes one program over one input and answers a JSON envelope. One-shot by
design — no feed, no edit, no follow — with fixed ledger ceilings (256 MiB
memory, 64 MiB output) since a browser tab has no governor. The
[playground](assets/playground/) on this site is that binding running in your
browser.

## Governance is not optional

There is no ungoverned entry. `ResourceLimits` (input, output, memory, spill,
nesting), the work meter, and cancellation ride every execute; `jqf_new_limited`
exposes the same dials over FFI; the wasm binding pins them. An embedded runaway
is a typed refusal, exactly like the CLI's — see
[Usage: memory](usage.md#memory-and-residency).
