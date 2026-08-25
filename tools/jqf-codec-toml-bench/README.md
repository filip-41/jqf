# jqf-codec-toml-bench

Retained TOML codec worker: jqf decode/encode lanes plus the `toml` and
`toml_edit` competitors, same fixtures.

What it has:

- whole-document jqf decode (physical route receipt asserted)
- deterministic jqf encode
- `toml` / `toml_edit` competitor decode
- one scoped-route lane over `medium/mixed`
- untimed correctness preflight before timing

```text
cargo run --release -p jqf-codec-toml-bench
```

Release-only (the shared harness refuses a debug build). Not a standing gate.
Depends on `jqf-bench-core`.
