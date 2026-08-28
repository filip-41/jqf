# Parallelism

jqf's parallelism holds one law, the detail under
[Architecture § Parallelism](architecture.md#parallelism):

> A parallel answer is **byte-identical** to a serial one, or the whole request
> re-runs serially. There is no partially-ordered mode and no worker-visible
> nondeterminism.

## What runs in parallel

Two input shapes, both record-like:

- **Explicit NDJSON**, with morsels cut at the framer's record boundaries.
- **Default adjacent-values stdin**, with morsels cut only at provable top-level
  value boundaries (depth zero, outside strings).

No format is detected and no dialect is selected to get there; ineligible inputs
simply plan serial. `--parallel` is on by default and `--workers N|auto` sizes
the pool.

## Morsels

A **morsel** is a contiguous byte range of whole records, never a single record
and never a partial one. Policy: at least 128 KiB, at most 4 MiB per morsel,
aiming at four morsels per worker so the tail balances.

Workers run the same compiled program over their morsels independently. The
coordinator republishes results **in ordinal order**, and only *clean* morsels
(published bytes and nothing else). The moment anything else happens — a
per-value error, a decode issue, a worker fault — the parallel attempt yields:
its clean prefix is discarded and the whole request re-runs serially. Errors
therefore never have to be merged, ordered, or attributed across workers.

A resource refusal (the memory ceiling) is **not** a yield. It is the same typed
stop it would be serially.

## `--workers auto`

- Below **256 KiB** of input, parallelism cannot pay for itself: serial.
- Above it, the width is how many minimum-size morsels the input yields, clamped
  between 2 and the core ceiling (P-cores plus half the E-cores, detected once).
- An explicit `--workers N` is clamped to 1…256.

Small inputs therefore stay serial without a flag, which is why wall time on a 2
KiB file is process startup, not scheduling.

## What disqualifies a program

Every decline has a named reason. The families:

| Reason            | Examples                                                                                                                  |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------- |
| program shape     | impure builtins (`input`, `now`, `uuid`, `stderr`), the [engine surface](generators.md), `-e` (exit rides the last value) |
| single-run models | `-n`, `-s` — one run, nothing to shard                                                                                    |
| stateful streams  | headered CSV (the header is whole-stream state)                                                                           |
| output models     | non-JSON-family output, `--edit`, `--split-exp`, colored output, `--unbuffered`                                           |
| host bindings     | `--arg` family, `-L`                                                                                                      |
| input too small   | below break-even, or a single morsel                                                                                      |

`--explain` shows both halves: `class: … morsel_static=…` (is the program a
static per-record path) and `ladder: morsel=yes|no` (is every call in it pure
and therefore morsel-eligible).

## The runtime owns the threads

`jqf-runtime` is the only crate that spawns threads. Workers are scoped threads,
joined before the drive returns and cancelled on drop, so no request outlives
its call. A worker receives a numeric memory grant carved from the parent
request's ledger (morsel-sized envelopes), never a live permit, and budgets are
not `Send` by construction.

`--max-rss` watches the whole process (all workers together), and a grant
degradation shrinks the worker count toward serial rather than failing the
request. See [Memory and limits](memory.md).
