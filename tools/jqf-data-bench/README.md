# jqf-data-bench

Release-only worker for representative successful `jqf-data` paths.

What it has:

- Timing suite via `jqf-bench-core` (`--quick`, `--json`, `--filter`)
- Separate allocation mode (`--features allocation-stats -- --allocations`)
- Untimed deterministic preflight on every selected case
- Frozen inventory of value, object, document, reader, and JSON decode lanes

## Run

```sh
cargo run --release --locked -p jqf-data-bench
cargo run --release --locked -p jqf-data-bench -- --quick --filter object/lookup
cargo run --release --locked -p jqf-data-bench -- --json
```

Allocation accounting is a separately compiled path:

```sh
cargo run --release --locked -p jqf-data-bench \
  --features allocation-stats -- --allocations --json
```

Zero byte declarations mean the case has no honest logical byte stream; the
reported operation rate remains meaningful.

This crate depends on `jqf-bench-core`, which is not a member of this
worktree. Point `workspace.dependencies.jqf-bench-core` at that crate before
building.
