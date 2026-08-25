# jqf-resource

Defines request limits, usage accounting, and cooperative work budgets.

What it has:

- `ResourceLimits` and `ResourceError` — ceilings and why a charge was refused
- `RequestAccount` — one request's usage account
- `CountingAlloc` — process allocator that charges heap allocations to the
  request account
- `OutputPermit` — reservation against the output ceiling
- `Control` — cancel, deadline, and host memory stop
- `WorkMeter` — work budget for one cooperative slice
- `task::TaskGrantBudget` / `task::TaskChildAccount` — a child account that may return one
  result when it is quiet
- `DiagnosticRecord` / `DiagnosticBuffer` — diagnostic events and a capped buffer
- `ResourceContext` — account, control, and work budget for one request

## Limits

`ResourceLimits` holds independent ceilings. Charge paths use checked
arithmetic.

```rust
use jqf_resource::ResourceLimits;

let limits = ResourceLimits::new(u64::MAX, 4096, 1 << 20, u64::MAX, 100);
assert_eq!(limits.max_output_bytes(), 4096);
assert_eq!(limits.max_memory_bytes(), 1 << 20);
```

## Usage account

`RequestAccount` owns one request's counters. Opening it charges its own
allocation as `Retained`.

`MemoryCategory` labels (`Retained`, `Working`, …) share one memory ceiling.
They are for reports, not separate budgets.

```rust
use jqf_resource::{MemoryCategory, RequestAccount, ResourceLimits};

let limits = ResourceLimits::new(u64::MAX, 4096, 1 << 20, u64::MAX, 100);
let account = RequestAccount::try_new(limits).unwrap();
assert_eq!(
    account.snapshot().memory(MemoryCategory::Retained).current(),
    RequestAccount::minimum_memory_bytes()
);
```

## Counting allocator

`CountingAlloc` is the process allocator. Install the request account on
the thread (`RequestAccount::try_share`) so heap allocations charge that
account from their `Layout`. Ordinary allocations must be freed under the
same ambient account; use task result buffers for supported cross-thread
transfer.

A refused allocation can use the available emergency slab instead of
aborting. The slab rewinds once no ambient scope or slab-backed block
remains, immediately at scope teardown or when the next scope starts.

This crate links `std` because the allocator, the thread-local account,
and the emergency slab need it.

## Output

`ResourceContext` holds one request's account, control, and work budget.

`OutputPermit` reserves bytes against the output ceiling before anything
is published. `commit(n)` records `n` published bytes and releases the
unused part of the reservation. Dropping the permit releases all of it.

```rust
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

static CONTROL: ContinueControl = ContinueControl;

let limits = ResourceLimits::new(u64::MAX, 4096, 1 << 20, u64::MAX, 100);
let resources = ResourceContext::new(
    RequestAccount::try_new(limits).unwrap(),
    &CONTROL,
    WorkMeter::try_new_v1(4096).unwrap(),
)
.unwrap();

let permit = resources.reserve_output(512).unwrap();
permit.commit(128).unwrap();
assert_eq!(resources.snapshot().output_bytes(), 128);
```

## Control

`Control` is how the host says cancel, deadline, or too much physical
memory. `ContinueControl` always continues. Running out of work budget is
`WorkAdmission::Pending`, not a control error.

## Work budget

`WorkMeter` starts each cooperative slice with a fresh budget. When the
budget is gone, you get `WorkAdmission::Pending`, not a resource failure.
Unused budget does not carry into the next slice.

```rust
use jqf_resource::WorkMeter;

let meter = WorkMeter::try_new_v1(8).unwrap();
assert_eq!(meter.remaining(), 8);
assert!(WorkMeter::try_new_v1(0).is_none());
```

## Worker grants

`reserve_task_grant` returns a reservation and a `TaskGrantBudget` that may
cross to exactly one worker. The worker opens a `TaskChildAccount` from those
numbers and may send back one result only when that child is quiet.

## Diagnostics

A `DiagnosticRecord` is a code plus a few small fields. `DiagnosticBuffer`
keeps a fixed cap of records, newest wins; the marked failure stays, and
overflow is counted. `Severity` comes from `jqf-source`.

```rust
use jqf_resource::diag::{DiagnosticBuffer, DiagnosticRecord, DiagnosticSink, codes};

let buffer = DiagnosticBuffer::with_cap(1);
buffer.record(DiagnosticRecord::new_registered(codes::ROUTE_SELECTED));
assert_eq!(buffer.records()[0].code_name(), "ROUTE_SELECTED");
```

## Contracts

See [`CONTRACTS.md`](CONTRACTS.md) for the accounting rules.
