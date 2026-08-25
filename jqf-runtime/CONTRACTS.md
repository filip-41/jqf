# jqf-runtime Contracts

Invariants for this crate and for hosts. Type overview and examples live
in [README.md](README.md).

This crate does not parse programs, compile them, or own a document
codec. It is the native host: threads, the filesystem, and the two
morsel drives that call into `jqf-engine` and the codec catalog.

It depends on `jqf-resource`, `jqf-engine`, `jqf-sdk`, `jqf-source`,
`jqf-data`, and the codec crates. The portable grant and work-budget
laws live in `jqf-resource`; this crate sizes and spends them.

## Host

- This crate is the only crate that may spawn a thread or open a temp
  directory. Portable crates stay `no_std` and allocator-agnostic.
- Worker capacity is supplied by the host or session. Portable code
  never reads process environment or machine topology.
- `WorkerBudget` and `WorkerPermit` are not `Send`. A permit cannot
  cross to another thread and drop its slot there.
- `NativeWorkerHost` starts work only under a live `NativeWorkerScope`.
  The worker receives a numeric grant, never the coordinator permit.

## Grants

- A worker envelope is sized from the morsel window (128 KiB..4 MiB),
  never from the record stream's `max_record_bytes` and never from the
  SDK batch bound. Sizing from the record ceiling would make one
  envelope as large as the input and refuse every `N >= 2`.
- `reserve_record_worker_grants` stops at the first envelope the parent
  ceiling refuses. A refused grant is not an error: the report records
  how many were granted, including zero (the serial path). The live
  relay reserves per morsel inside the coordinator and reports
  degradation on its own line; this helper is the batch receipt.
- The parent ceiling binds the sum of every in-flight envelope because
  every envelope is an ordinary reservation against it.
- A worker may return one result only when its child account is quiet.
  The parent adopts the published prefix and shrinks the charge to it.

## Plan

- `ParallelPlan` decides width and morsel size. It starts no thread and
  touches no ledger.
- `auto` stays serial below the route's own break-even. Each route
  owns its `WidthPolicy`; copying another route's numbers is claiming
  evidence it does not have.
- Explicit `--workers N` is clamped to `1..=WORKER_HARD_CAP` (256).
  Oversubscription below the cap is a supported measurement mode.
- `auto`'s ceiling is P-cores plus half the E-cores, cached once per
  process. `--workers N` still reaches the hard cap.
- A request plans serial when the program, input, output, or host
  bindings cannot cross a morsel worker. The decision label says which
  gate fired.

## Morsels

- A morsel is a contiguous record *range* sized by bytes, never a
  single record. One coordinator ordinal is one morsel.
- The coordinator relays only a clean morsel: published bytes and
  nothing else. An issue, a per-value runtime error, a drive failure,
  or a worker fault is an ordered terminal and yields the rest of the
  request to serial.
- Yield-to-serial reruns the whole request on the serial drive and
  discards the clean prefix already published. A worker never renders
  diagnostics, input line numbers, or an exit class from a byte range
  of the stream. A memory-ceiling or host-control refusal is not a
  yield: the request stops typed and the remainder is not re-run.
- Headered CSV is stream-stateful: record zero names every later
  record. That kind is fenced off the morsel lane.
- Adjacent-value shards start only at a `}` or `]` that returns the
  scan's nesting depth to zero outside a string. A failed or
  diagnostic-emitting shard yields the request to serial.

## Ordering

- Released output is the concatenation of clean morsels in ordinal
  order. The byte stream equals serial's byte stream for that input.
- The coordinator never reorders, drops, or splits a clean morsel.
- Colour, `--unbuffered`, `--split-exp`, `-e`, and a stateful output
  dialect all need per-item identity the relay does not have, so they
  plan serial.

## Feed

- One feed is one program over one framed record stream.
- Everything after the last physical terminator is held. It is never
  framed, faulted, or emitted until more bytes arrive or `finish` is
  called.
- `poll` publishes one batch. A too-small buffer is a detectable
  re-call and re-delivers the same batch, never the next one.
- A strict-profile framing fault is terminal: later polls report
  `Failed`. A per-value runtime error is not: the drive records it and
  continues.
- The published prefix is drained after each delivered batch. Retained
  input stays bounded by the current batch plus the held partial
  record.

## Spill

- The store owns one private directory named `jqf-spill-` plus a
  16-byte hex suffix. Creation is exclusive at mode `0700`, never
  `create_dir_all`. A pre-existing path is `HostFailure`, never an
  adoption.
- The directory is created lazily at the first `create_run`.
- Each run is `create_new` at mode `0600`, then unlinked. Reads clone
  the held handle and read positionally. A sort key never has a name
  on disk after the write.
- Drop removes the directory. The CLI additionally pre-renders the
  path into an async-signal-safe cleanup handler for deaths `Drop`
  cannot see.
