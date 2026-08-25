# jqf-source-bench

Release-mode harness for representative `jqf-source` operations. Not a
substitute for the crate's source-position, patch, or diagnostic contract tests.

What it has:

- `patch/apply-4m-sparse` — 1,024 ordered non-overlapping sparse patches on 4 MiB
- `diagnostic/build-typical` — one ordered diagnostic with one source and two labels
- untimed correctness preflight before timing or allocation accounting
- receipts that keep fixture size, counts, spans, and checksum

## Run

```sh
cargo run --release --locked -p jqf-source-bench
cargo run --release --locked -p jqf-source-bench -- --quick --filter patch
cargo run --release --locked -p jqf-source-bench -- --json
```

Allocation path (distinct build — the measuring allocator would distort timing):

```sh
cargo run --release --locked -p jqf-source-bench \
  --features allocation-stats -- --allocations --quick
```

## Workloads

One logical patch operation is one patch; declared bytes are the 4 MiB input of
the complete apply. The diagnostic case declares 72 owned message and label
bytes one `build()` copies.

Timed invocations include output destruction. The patch fixture is immutable.
Compare results only across equivalent builds and environments.

Diagnostic checksums are allocation-free. The patch hot path keeps output
length and deterministic samples as a compact receipt; full output hashing
stays in the untimed preflight.
