# jqf-codec-json

JSON family codecs: RFC 8259, JSONC, JSON5, NDJSON, and json-seq (RFC 7464).

This crate is `no_std` and uses `alloc`. It depends on `jqf-codec-core` for
the route contracts, `jqf-source` for spans, `jqf-resource` for the work
budget, and `jqf-data` for documents and values.

What it has:

- `registration()` — the RFC 8259 full-document and encode routes
- `jsonc` / `json5` — commented and JSON5 dialects
- `ndjson` / `seq` — record streams (newline-delimited and RFC 7464)
- `JsonEncodeOptions` / `JsonIndent` — encode indent
- `json_escape_byte` / `push_json_escaped` — one-byte JSON escapes
- `FORMAT_ID`, `RFC8259_DIALECT_ID`, and the stable physical route ids

It does not evaluate programs or own I/O.

```rust
use jqf_codec_json::{FORMAT_ID, RFC8259_DIALECT_ID, registration};

assert_eq!(FORMAT_ID, "json");
assert_eq!(RFC8259_DIALECT_ID, "rfc8259");
assert!(registration().is_ok());
```

Family laws: [`CONTRACTS.md`](../CONTRACTS.md).
