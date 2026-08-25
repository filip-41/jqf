# jqf-resource-bench

Release-mode timing harness for successful `jqf-resource` paths.

What it has:

- `account/create-drop` — open an account and read its ledger-only snapshot
- `work/cooperative-transitions` — 65,536 transitions across 256 cooperative entries
- `nesting/enter-drop` — 4,096 sequential enter/drop cycles
- `output/reserve-partial-commit` — 4,096 reservations of 1 KiB, commit 768 bytes
- `reference-vec/push-65536-u64` — native `Vec` write with exact capacity
- `reference-string/append-1m` — native `String` write with exact capacity

Not a limit, cancel, or correctness suite. Those live in crate tests.

## Run

```sh
make resource-bench
```

Short profile, JSON, or one named lane:

```sh
cargo run --release --locked -p jqf-resource-bench -- --quick
cargo run --release --locked -p jqf-resource-bench -- --json
cargo run --release --locked -p jqf-resource-bench -- --filter reference-vec
```

Allocation accounting is a separately compiled mode so the allocator wrapper
never perturbs timing:

```sh
cargo run --release --locked -p jqf-resource-bench \
  --features allocation-stats -- --allocations
```

## Lanes

Each selected lane first runs an untimed receipt that checks `UsageSnapshot`
counters, final accounting after guards drop, cooperative control observations,
and deterministic checksums. The timed path is only the named operation plus a
fixed nonallocating checksum. Each timed invocation builds and drops fresh
request state.

The former `tracked-*` collection lanes are gone: `jqf-resource` no longer
exposes that API. Reference `Vec`/`String` lanes carry the collection-write
picture.

This crate depends on `jqf-bench-core` (workspace path `tools/jqf-bench-core`).
That crate is not in this tree until it is ported.
