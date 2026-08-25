# jqf-runtime

Owns threads, the filesystem, and the host-side drives for jqf.

This crate is the only crate that may spawn a thread or open a temp
directory. The portable halves live in `jqf-resource` and the codec
crates. It does not parse programs or encode documents on its own: the
drives call `jqf-engine` and the codec catalog.

What it has:

- `JsonItemSuffix` — the JSON facade item-suffix law
- `feed::{ResidentFeed, FeedPoll}` — incremental record feed
- `parallel::{WorkerRequest, ParallelPlan, PlanDecision, WORKER_HARD_CAP, core_topology}`
- `records::{RecordInputKind, RecordDriveSpec, execute_record_request}`
- `values::{ValueDriveSpec, execute_value_request, partition_request}`
- `spill::TempDirSpillStore` — create-then-unlink spill store
- `workers::{RecordWorkerEnvelope, reserve_record_worker_grants, NativeWorkerHost, OrderedRecordCoordinator, MorselByteRange}`

## Item suffix

`--raw-output0` replaces the suffix with NUL and wins over `-j`. Non-JSON
targets never use this type. NDJSON / json-seq / CSV / TSV: the codec owns
framing. Render / YAML / markup: the facade writes LF.

```rust
use jqf_runtime::JsonItemSuffix;

assert_eq!(JsonItemSuffix::from_dials(false, false).as_bytes(), b"\n");
assert_eq!(JsonItemSuffix::from_dials(false, true).as_bytes(), b"");
assert_eq!(JsonItemSuffix::from_dials(true, true).as_bytes(), b"\0");
```

## Worker grants

A grant envelope is sized from the morsel window, never from the record
ceiling. Reservation stops at the first envelope the parent ledger
refuses and reports the degradation.

```rust
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_runtime::workers::{RecordWorkerEnvelope, reserve_record_worker_grants};

static CONTROL: ContinueControl = ContinueControl;
let limits = ResourceLimits::new(u64::MAX, 1 << 20, 1 << 26, u64::MAX, 100);
let resources = ResourceContext::new(
    RequestAccount::try_new(limits).unwrap(),
    &CONTROL,
    WorkMeter::try_new_v1(4096).unwrap(),
)
.unwrap();

let envelope = RecordWorkerEnvelope::try_new(128 << 10, RecordWorkerEnvelope::MEASURED_FIXED_BYTES).unwrap();
let grants = reserve_record_worker_grants(4, envelope, &resources).unwrap();
assert!(!grants.report().degraded());
assert_eq!(grants.report().granted(), 4);
```

## Parallel plan

`ParallelPlan` is the printable `--workers` decision. Nothing in the
planner starts a thread. `WORKER_HARD_CAP` is 256; an explicit width
above it is clamped.

```rust
use jqf_runtime::parallel::{ParallelPlan, PlanDecision, WORKER_HARD_CAP, WorkerRequest};

assert_eq!(WORKER_HARD_CAP, 256);
let plan = ParallelPlan::serial(WorkerRequest::Auto, PlanDecision::BelowBreakEven, 1024);
assert!(!plan.is_parallel());
assert_eq!(plan.workers(), 0);
assert_eq!(plan.decision(), PlanDecision::BelowBreakEven);
```

## Record kinds

A headered CSV kind is stream-stateful: record zero names every later
record, so the morsel lane refuses it.

```rust
use jqf_runtime::records::RecordInputKind;

assert!(!RecordInputKind::Ndjson.is_stream_stateful());
assert!(RecordInputKind::Csv { header: true, tsv: false }.is_stream_stateful());
assert!(!RecordInputKind::Csv { header: false, tsv: true }.is_stream_stateful());
```

A morsel is a contiguous byte range, never one record. The coordinator
relays only a clean morsel: published bytes and nothing else.

```rust
use jqf_runtime::workers::MorselByteRange;

let range = MorselByteRange::try_new(0, 32).unwrap();
assert_eq!(range.len(), 32);
assert!(range.fits_within(32));
assert!(MorselByteRange::try_new(8, 4).is_none());
```

## Feed

`ResidentFeed` is one program over one framed record stream whose input
arrives in pieces. `try_push` holds everything after the last physical
terminator. `poll` drives one completed batch and publishes with the
`snprintf` required-size convention: a too-small buffer is a re-call,
and re-polling re-delivers the same batch.

A per-value runtime error is not terminal. A strict-profile framing
fault is: later polls report `FeedPoll::Failed`.

## Spill

`TempDirSpillStore` owns one private directory, created lazily at the
first run. Each run is created then unlinked, so a sort key never has a
name on disk. Drop removes the empty directory.

```rust
use jqf_runtime::spill::TempDirSpillStore;

let store = TempDirSpillStore::try_new(None).unwrap();
assert!(store.temp_dir().file_name().unwrap().to_str().unwrap().starts_with("jqf-spill-"));
```

## Contracts

See [`CONTRACTS.md`](CONTRACTS.md) for the host, grant, and drive invariants.
