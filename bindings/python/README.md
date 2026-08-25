# jqf (Python)

The Python binding: ctypes over the C ABI. A failed run gives a
`Record`, not a string.

This package does not compile a native extension. It loads
`libjqf_sdk_ffi` and talks to the checked-in header. The wheel build
bundles that library so an install does not need a checkout.

What it has:

- `run` / `run_many` — one-shot compile and run
- `Session` / `Program` — compile once, run many
- `Feed` — push input in pieces, poll bounded batches
- `Record` / `JqfError` / `JqfTruncatedError` — structured outcomes
- `codes` — the diagnostic-code table

```python
import jqf

result = jqf.run(".n + 1", b'{"n":1}')
assert result.ok
assert result.output.strip() == b"2"
```

## One-shot

`run` is one document. `run_many` is an adjacent-value stream. Both
return a `RunResult`. A terminal failure raises `JqfError` with the
record.

Output follows the snprintf convention: a required size larger than
the offered buffer is the exact size of the next call, not a truncated
success.

## Session

`Session` holds one engine handle and one program table. `compile`
returns a `Program` id that dies with the session. A stale id is
`JqfError`, never undefined behavior.

```python
import jqf

with jqf.Session() as session:
    program = session.compile(".n + 1")
    assert program.run(b'{"n":1}').output.strip() == b"2"
```

`compile_args` binds host values as `$name` constants. The engine
parses the JSON; nothing is spliced into the source.

The handle is one thread at a time. A host that wants parallelism
uses one `Session` per thread.

## Feed

`open_feed` starts a resident record stream on a `Program`. `push`
appends bytes. `poll` returns one bounded batch, or empty when
nothing is complete. `finish` marks end of input so a held tail
becomes the last record.

`"strict"` stops on the first framing fault. `"recovering"` keeps
going; issues ride `last_diagnostics()`.

## Records

A `Record` carries code, class, severity, locators, and optional
kind / operand / payload. `render()` is the text for that record.
`RunResult.diagnostics` is the stream for that run, oldest first.

## Library path

Load order: `JQF_FFI_LIB`, then a copy next to the package, then
`target/release`, then the loader path. The binding checks
`jqf_abi_version` at import and refuses a mismatch.

## Contracts

See [`CONTRACTS.md`](CONTRACTS.md) for ABI, session, and buffer
invariants.
