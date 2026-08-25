# jqf-codec-toml

TOML 1.0 and 1.1 decode and encode for jqf.

This crate is `no_std` and uses `alloc`. It depends on `jqf-codec-core`,
`jqf-data`, `jqf-resource`, and `jqf-source`. It owns the TOML grammar.

What it has:

- `registration_1_0` / `registration_1_1` — catalog entries
- whole-document decode and an exact-path located route
- deterministic semantic encode (`toml.jqf-1.0@1` / `toml.jqf-1.1@1`)
- `FORMAT_ID` and the dialect / route id constants

```rust
use jqf_codec_toml::{FORMAT_ID, registration_1_0};

let registration = registration_1_0().unwrap();
assert_eq!(registration.descriptor().format().as_str(), FORMAT_ID);
```

Family laws: [`CONTRACTS.md`](../CONTRACTS.md).
