# jqf-wasm Contracts

Invariants for this crate and for hosts. Type overview and examples live
in [README.md](README.md).

This crate does not parse a format grammar, open a file, or evaluate a
program itself. It is the WebAssembly facade over `jqf-sdk`,
`jqf-engine`, and `jqf-runtime`.

`jqf.js` is the checked-in wrapper. The `wasm-bindgen` glue is generated
at build time and is not a source file.

## ABI version

- `ABI_VERSION` is 1. `jqf.js` calls `jqf_abi_version` at load and
  refuses a mismatch.
- A bump is any change to an export's signature, meaning, or the
  envelope shape.

## Session

- The first `jqf_run` builds one session and leaks it for the instance's
  life. Later calls reuse it.
- The instance is one thread. The session lives in a thread-local.
- Ledger ceilings are fixed: 256 MiB memory, 64 MiB output, 64 MiB
  spill, nesting depth 1_000. There is no deadline: this target has no
  wall clock.

## Envelope

- `jqf_run` returns a JSON object. It does not fail across the
  boundary. Setup and run failures are `ok:false` with `error`.
- `output` is the published prefix, including on failure.
- Non-UTF-8 published bytes set `binary` and `output_base64`. They do
  not set `output` to a lossy string.
- `records` is the diagnostic stream for this call. Each call clears
  the stream first.
- `value_errors` is the per-value error list, in input order.

## Indent and flags

- `indent` is `-1` (tabs), `0` (compact), or `1..=7` (spaces). Any
  other width is a setup failure. It is never clamped.
- Null input beats slurp. That precedence is the same as the CLI.

## Formats

- The format table is closed. An unknown name is a setup failure.
- A `record_input` name decodes only through the record drive. Serving
  it on the adjacent-value ladder is a contract break.
- Record inputs publish json, ndjson, json-seq, csv, tsv, yaml, xml,
  and html. Any other output name is a setup failure.
- The record drive is planned serial. There is no worker pool.

## Scope

- One-shot only: compile and run per call. No feed, edit, diff, or
  follow.
- `$ARGS` binds empty.
- User input never panics. A panic means the committed format table
  named an unknown format.
