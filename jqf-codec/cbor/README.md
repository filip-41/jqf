# jqf-codec-cbor

CBOR (RFC 8949) decode and encode for jqf.

This crate is `no_std` and uses `alloc`. It depends on `jqf-codec-core`,
`jqf-data`, `jqf-resource`, and `jqf-source`. It owns the CBOR grammar.

What it has:

- `registration` — one catalog entry carrying the input dialect
  (`cbor.rfc8949-generic@1`) and four output profiles (source, preferred,
  core-deterministic, length-first), plus the `cbor-seq` sequence framing
- whole-document decode, an exact-path located route, and a scoped walk
- deterministic semantic encode under the four dialect ids
- RFC 8949 §5.6.1 map-key uniqueness enforcement and the §4.2.2
  simple-value registry
- map-key projection narrowing: a map with any non-text key is refused with
  `UnsupportedRepresentation` (a documented narrowing, not a spec error)

```rust
use jqf_codec_cbor::{FORMAT_ID, registration};

let registration = registration().unwrap();
assert_eq!(registration.descriptor().format().as_str(), FORMAT_ID);
```

Family laws: [`CONTRACTS.md`](../CONTRACTS.md).
