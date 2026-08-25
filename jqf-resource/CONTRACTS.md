# jqf-resource Contracts

Invariants for this crate and for hosts. Type overview and examples live
in [README.md](README.md).

This crate does not parse, evaluate, encode, schedule workers, read
clocks, or measure process RSS.

## Policy

`ResourceLimits` holds independent ceilings. Charge paths use checked
arithmetic. Some diagnostic counters saturate; `force_residency` saturates.

`MemoryCategory` labels why memory is held. Labels share one memory
ceiling. They do not create separate budgets.

The spill-bytes *limit* is a ceiling on in-memory sort keys. This crate
does not count that dimension — there is no `charge_spill`. Spill-*disk*
bytes are counted, opt-in (`0` means unset), cumulative, and never
released.

## Accounting

- `RequestAccount` uniquely owns one fallibly allocated request ledger.
- The ledger allocation itself is charged on that new ledger as Retained.
  Creating an account does not charge whoever is already ambient (a parent
  must not pay Working for a child's `AccountBox`).
- The permit accessor `OutputPermit::reserved_bytes` and the account
  residency pair `charge_residency` / `release_residency` are deliberate
  test surface for inert permits and direct ledger invariants.
- The counting global allocator is the memory accountant: every heap
  operation in an installed ambient scope is charged at `GlobalAlloc` from
  the exact `Layout`.
- Allocation attribution is thread-local. Ordinary heap storage must be
  deallocated under the same ambient account that allocated it; `GlobalAlloc`
  provides no origin on deallocation, so arbitrary cross-account transfers
  cannot preserve exact live counters. Task result buffers are the supported
  transfer path and carry their own release policy.
- A request that meters memory installs a *shared handle* to its context
  account as ambient (`RequestAccount::try_share`). Heap charges and grant
  holds then land on the same cells. Release clamps a free that was never
  charged, so a pre-scope allocation cannot underflow the ledger.
- An ambient scope is one request. `install` saves and restores the previous
  account and trip latch, nothing more. The emergency slab is per-process
  while a request is live: the first tripper owns the bump, a tripped
  request is terminal, and a live slab-backed block keeps the bump claimed
  after its scope drops. The bump rewinds when no outermost scope or live
  slab block remains: at the last scope drop when already empty, or at the
  next outermost install after the final block is freed. Rewind does not
  memset; `alloc_zeroed` zeros the new region.
- The host installs ambient only to enforce a ceiling or to report a peak.
  Parallel workers install their own child account; the request thread does
  not install just because the parallel switch is on. Default-parallel RSS
  is the `--max-rss` governor, not the ledger; a finite memory ceiling
  already forces the parent install.
- The first thread that trips the memory ceiling owns the emergency slab;
  later siblings get null. Exhaustion is null: new growth after the trip
  is latched is a typed allocation failure, not a booked real-heap block.
  `alloc`, `alloc_zeroed`, and growing `realloc` share that path.
- A refused grow that moves a live heap block onto the slab deallocates
  the old heap block after the copy. Failure leaves the old allocation
  unchanged.
- A `Vec` whose charge died with the child ledger (an adopted or detached
  worker result built under an installed child ambient) is dropped with
  ambient release suppressed; only the grant hold is released on the
  parent. A result charged on a still-live ambient ledger (no child
  ambient was installed) releases its PendingIo on drop instead.
- Usage snapshots observe current and peak counters without acquiring
  allocation ownership. Admission commits in the same step.
- Output permits count against the aggregate output ceiling before host
  publication without counting as published bytes. Commit publishes exactly
  one prefix and releases the suffix; drop rolls all authority back.

## Cooperative control

- `Control` is how the host says cancel, deadline, or too much memory.
- Every fresh cooperative entry checks control before replacing the work budget.
- Work exhaustion returns `Pending`, not a resource failure.
- Unused budget never leaks into the next cooperative entry.
- Publishing externally visible progress requires a final control check.
- Nested operations borrow the same `ResourceContext`.

## Request context

`ResourceContext` also holds environment, cwd, search paths, stderr, the
diagnostic sink, the spill store, one extra host object, the `rand` seed,
precision/projection counters, the mismatch knob and its per-kind counts,
and the strictness knob. The context fields are not charged to the ledger;
`DiagnosticBuffer` explicitly charges retained storage as Diagnostic
memory, while other sinks own their allocation category.

`set_lazy_deferred_spans` overwrites the previous value. A reused
context must call `reset_run_diagnostics` at the start of each run so
the previous run's counters cannot leak through.

`MismatchPolicy`, `StrictnessPolicy`, the mismatch table, and
`ProjectionKind` live in `policy` so this crate does not depend on anything
above it.

## Task grants

`reserve_task_grant` returns a coordinator-local reservation and a linear,
non-`Clone` numeric budget that may cross to exactly one worker. The worker
opens its own child ledger from the numbers (no parent pointer) and binds it
to worker-local control and work state.

Detachment is permitted only from a quiet child: no reserved output, no
nesting, no Working/Diagnostic residency, Retained equal to the ledger,
and live memory no more than the ledger plus the result capacity.

Adoption re-attaches the exact allocation capacity to the parent from the
reservation's output component while every unused component releases.
Every terminal path — adoption, refusal, worker panic, worker cancel —
returns the parent's current+reserved to where it started.

A child ledger refuses parent-only input and spill-disk charges. Those
accessors report `u64::MAX` on a task account so a deref to
`ResourceContext` would otherwise admit them unbounded. Child output
permits enforce their prefix but remain inert ledger successes because the
grant's output authority owns that bound.

Grant identity is one process-wide counter. The reservation, child account,
child context, output buffer, and adopted result are request-local (`!Send`).
The budget and the detached buffer may cross a thread.

## Diagnostics

Diagnostic records are a code plus a few small fields, sent through a
host-supplied sink. No sink means emit does nothing. `ResourceContext`
adds no allocation or category scope around the call. `DiagnosticBuffer`
may retain and explicitly charges that storage as Diagnostic memory;
other sinks own their allocation category. `DiagnosticBuffer` keeps a
fixed cap of records, newest wins; the marked failure stays, and overflow
is counted.

## Boundaries

The only jqf dependency is `jqf-source`, for `Severity`. This crate does
not parse, evaluate, encode, pick routes, schedule workers, read clocks,
or measure process RSS.
