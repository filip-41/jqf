# jqf-codec-messagepack

MessagePack codec: one semantic input dialect, a key-equivalence variant, and
two deterministic output profiles.

This crate is `no_std` and uses `alloc`. It depends on `jqf-codec-core` for
the route contracts, `jqf-source` for spans, `jqf-resource` for the work
budget, and `jqf-data` for documents and values.

What it has:

- `registration()` — the MessagePack full-document and encode routes
- `MESSAGEPACK_UTF8_DIALECT_ID` / `MESSAGEPACK_KEY_EQUIVALENCE_DIALECT_ID` /
  `MESSAGEPACK_WIRE_DIALECT_ID` — input dialects
- `MESSAGEPACK_DETERMINISTIC_DIALECT_ID` /
  `MESSAGEPACK_DETERMINISTIC_FLOAT64_DIALECT_ID` — encode profiles
- `FORMAT_ID` and the stable physical route ids
- arbitrary-key maps project to objects only when every key is a `str`

It does not evaluate programs or own I/O.

```rust
use jqf_codec_messagepack::{FORMAT_ID, registration};

assert_eq!(FORMAT_ID, "messagepack");
assert!(registration().is_ok());
```

Family laws: [`CONTRACTS.md`](../CONTRACTS.md).
