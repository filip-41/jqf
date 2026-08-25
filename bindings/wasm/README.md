# jqf-wasm

The WebAssembly binding: run a program over bytes in a browser, over the
same SDK the CLI drives.

This crate links `std`. It depends on `jqf-sdk` for the request,
`jqf-engine` for compile, `jqf-runtime` for the record drive,
`jqf-resource` for the ledger, and the codec crates the session
registers. It does not parse a format grammar or open files.

What it has:

- `jqf_run` — compile and run one program; returns a JSON envelope
- `jqf_formats` — the closed format table as a JSON array
- `jqf_version` / `jqf_abi_version` — package string and numeric ABI
- `FLAG_*` — raw strings, sort keys, ASCII, null input, tab indent
- `jqf.js` — `loadJqf()` plus option defaults and envelope parsing

The instance is one thread. The first call builds one session and keeps
it for the instance's life.

```rust
use jqf_wasm::jqf_run;

let envelope = jqf_run(".n + 1", br#"{"n":1}"#, "json", "json", 0, 0, false);
assert!(envelope.contains("\"ok\":true"));
assert!(envelope.contains("2"));
```

## Envelope

`jqf_run` never fails across the boundary. Every outcome is JSON:

```text
{"ok":true,"output":"...","value_errors":[],"records":[...]}
{"ok":false,"output":"partial","error":"...","records":[...]}
```

UTF-8 output is a JSON string. Non-UTF-8 output is
`"binary":true` plus `output_base64`. Diagnostic records come from the
SDK stream. A halt may add `halt_status`.

## Indent and flags

`indent` is `-1` for tabs, `0` for compact, `1..=7` for spaces per
level. Any other width is `ok:false`. It is not clamped.

`flags` is a bitmask of the `FLAG_*` constants. Null input beats slurp.

```rust
use jqf_wasm::{FLAG_NULL_INPUT, jqf_run};

let envelope = jqf_run("type", b"1 2 3", "json", "json", 0, FLAG_NULL_INPUT, true);
assert!(envelope.contains("\\\"null\\\""));
```

## Formats

`jqf_formats` lists the closed name table. A name the table does not
carry is `ok:false`. Record-framed inputs (ndjson, json-seq, csv, tsv,
and the header twins) go through the record drive. Everything else
uses the value ladder.

A browser tab has no worker pool. The record drive is always serial.

## JavaScript

`jqf.js` loads the generated glue, checks the ABI, and wraps `jqf_run`:

```js
const jqf = await loadJqf();
const result = jqf.run('.name', '{"name":"Ada"}');
```

`input: null` sets the null-input flag. Binary results keep
`output_base64`.

## Contracts

See [`CONTRACTS.md`](CONTRACTS.md) for session, envelope, and format
invariants.
