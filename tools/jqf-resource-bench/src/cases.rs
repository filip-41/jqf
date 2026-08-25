//! Successful resource operations and their untimed correctness validation.

use std::{cell::Cell, hint::black_box, sync::OnceLock};

use jqf_bench_core::{BenchmarkCase, CaseMetadata, PreflightReceipt};
use jqf_resource::{
    ContinueControl, Control, ControlError, ControlOutcome, CooperativeError, MemoryCategory, RequestAccount,
    ResourceContext, ResourceError, ResourceLimits, UsageSnapshot, WorkAdmission, WorkMeter,
};

const U64_VALUES: usize = 65_536;
const STRING_BYTES: usize = 1_048_576;
const STRING_CHUNK_BYTES: usize = 1_024;
const WORK_TRANSITIONS: u32 = 65_536;
const WORK_ENTRY_CREDITS: u32 = 256;
const WORK_ENTRIES: u32 = WORK_TRANSITIONS / WORK_ENTRY_CREDITS;
const WORK_CONTROL_CHECKS: u32 = WORK_ENTRIES + 1;
const NESTING_LIFECYCLES: u64 = 4_096;
const OUTPUT_RESERVATION_BATCH: u64 = 4_096;
const OUTPUT_RESERVED_BYTES: u64 = 1_024;
const OUTPUT_PUBLISHED_BYTES: u64 = 768;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

static CONTINUE_CONTROL: ContinueControl = ContinueControl;
static VECTOR_FIXTURE: OnceLock<Vec<u64>> = OnceLock::new();
static STRING_CHUNK: OnceLock<String> = OnceLock::new();
static STRING_FIXTURE: OnceLock<String> = OnceLock::new();

#[derive(Clone, Copy, Debug)]
enum CaseKind {
    AccountCreateDrop,
    WorkCooperativeTransitions,
    NestingEnterDrop,
    OutputReservePartialCommit,
    ReferenceVecPush,
    ReferenceStringAppend,
}

impl CaseKind {
    const ALL: [Self; 6] = [
        Self::AccountCreateDrop,
        Self::WorkCooperativeTransitions,
        Self::NestingEnterDrop,
        Self::OutputReservePartialCommit,
        Self::ReferenceVecPush,
        Self::ReferenceStringAppend,
    ];

    const fn metadata(self) -> CaseMetadata {
        match self {
            Self::AccountCreateDrop => CaseMetadata::new("account/create-drop", 1, 0),
            Self::WorkCooperativeTransitions => {
                CaseMetadata::new("work/cooperative-transitions", WORK_TRANSITIONS as u64, 0)
            }
            Self::NestingEnterDrop => CaseMetadata::new("nesting/enter-drop", NESTING_LIFECYCLES, 0),
            Self::OutputReservePartialCommit => CaseMetadata::new(
                "output/reserve-partial-commit",
                OUTPUT_RESERVATION_BATCH,
                OUTPUT_RESERVATION_BATCH * OUTPUT_PUBLISHED_BYTES,
            ),
            Self::ReferenceVecPush => CaseMetadata::new(
                "reference-vec/push-65536-u64",
                U64_VALUES as u64,
                (U64_VALUES * size_of::<u64>()) as u64,
            ),
            Self::ReferenceStringAppend => CaseMetadata::new(
                "reference-string/append-1m",
                (STRING_BYTES / STRING_CHUNK_BYTES) as u64,
                STRING_BYTES as u64,
            ),
        }
    }
}

struct ResourceCase {
    kind: CaseKind,
}

impl BenchmarkCase for ResourceCase {
    fn metadata(&self) -> CaseMetadata {
        self.kind.metadata()
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let observation = run_operation(self.kind).map_err(operation_error)?;
        validate(self.kind, &observation)?;
        let hot_checksum = observation.checksum();
        let contents = validate_full_collection(self.kind)?;
        let checksum = contents.map_or(hot_checksum, |contents| {
            let mut checksum = Checksum(hot_checksum);
            checksum.mix_u64(contents);
            checksum.finish()
        });
        Ok(PreflightReceipt::new(
            checksum,
            format!(
                "{}: observation={observation:?} full_contents={contents:?} checksum=0x{checksum:016x}",
                self.kind.metadata().name
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        run_operation(self.kind)
            .expect("the successful resource operation failed after its preflight")
            .checksum()
    }
}

pub(crate) fn cases() -> Vec<Box<dyn BenchmarkCase>> {
    CaseKind::ALL
        .into_iter()
        .map(|kind| Box::new(ResourceCase { kind }) as Box<dyn BenchmarkCase>)
        .collect()
}

#[derive(Clone, Copy, Debug)]
enum Observation {
    Account {
        final_usage: UsageSnapshot,
    },
    Work {
        final_usage: UsageSnapshot,
        route: u64,
        entries: u32,
        checks: u32,
        final_credits: u32,
    },
    Nesting {
        final_usage: UsageSnapshot,
        route: u64,
    },
    OutputReservation {
        final_usage: UsageSnapshot,
        route: u64,
    },
    ReferenceVec {
        len: usize,
        capacity: usize,
        samples: VectorSamples,
    },
    ReferenceString {
        len: usize,
        capacity: usize,
        samples: StringSamples,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VectorSamples {
    first: u64,
    middle: u64,
    last: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StringSamples {
    first: u8,
    one_third: u8,
    two_thirds: u8,
    last: u8,
}

impl Observation {
    #[allow(
        clippy::too_many_lines,
        reason = "one exhaustive checksum keeps every fixed-size observation field visible"
    )]
    fn checksum(self) -> u64 {
        let mut checksum = Checksum::new();
        match self {
            Self::Account { final_usage } => {
                checksum.mix_u64(1);
                checksum.mix_snapshot(final_usage);
            }
            Self::Work {
                final_usage,
                route,
                entries,
                checks,
                final_credits,
            } => {
                checksum.mix_u64(4);
                checksum.mix_snapshot(final_usage);
                checksum.mix_u64(route);
                checksum.mix_u64(u64::from(entries));
                checksum.mix_u64(u64::from(checks));
                checksum.mix_u64(u64::from(final_credits));
            }
            Self::Nesting { final_usage, route } => {
                checksum.mix_u64(5);
                checksum.mix_snapshot(final_usage);
                checksum.mix_u64(route);
            }
            Self::OutputReservation { final_usage, route } => {
                checksum.mix_u64(11);
                checksum.mix_snapshot(final_usage);
                checksum.mix_u64(route);
            }
            Self::ReferenceVec { len, capacity, samples } => {
                checksum.mix_u64(8);
                checksum.mix_usize(len);
                checksum.mix_usize(capacity);
                checksum.mix_vector_samples(samples);
            }
            Self::ReferenceString { len, capacity, samples } => {
                checksum.mix_u64(9);
                checksum.mix_usize(len);
                checksum.mix_usize(capacity);
                checksum.mix_string_samples(samples);
            }
        }
        checksum.finish()
    }
}

#[derive(Clone, Copy, Debug)]
enum OperationError {
    Resource(ResourceError),
    Control(ControlError),
    InvalidFixedWorkCredits,
    UnexpectedAdmission,
    UnexpectedEntryRejection,
}

impl From<ResourceError> for OperationError {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

impl From<ControlError> for OperationError {
    fn from(error: ControlError) -> Self {
        Self::Control(error)
    }
}

impl From<CooperativeError> for OperationError {
    fn from(error: CooperativeError) -> Self {
        match error {
            CooperativeError::Control(error) => Self::Control(error),
            CooperativeError::Memory(error) => Self::Resource(error),
        }
    }
}

fn operation_error(error: OperationError) -> String {
    match error {
        OperationError::Resource(error) => {
            format!("successful resource operation failed unexpectedly: {error:?}")
        }
        OperationError::Control(error) => {
            format!("successful control operation failed unexpectedly: {error:?}")
        }
        OperationError::InvalidFixedWorkCredits => "fixed benchmark work credits were invalid".to_owned(),
        OperationError::UnexpectedAdmission => "successful transition workload unexpectedly became pending".to_owned(),
        OperationError::UnexpectedEntryRejection => {
            "valid cooperative entry replenishment was unexpectedly rejected".to_owned()
        }
    }
}

fn run_operation(kind: CaseKind) -> Result<Observation, OperationError> {
    match kind {
        CaseKind::AccountCreateDrop => account_create_drop(),
        CaseKind::WorkCooperativeTransitions => work_cooperative_transitions(),
        CaseKind::NestingEnterDrop => nesting_enter_drop(),
        CaseKind::OutputReservePartialCommit => output_reserve_partial_commit(),
        CaseKind::ReferenceVecPush => Ok(reference_vec_push()),
        CaseKind::ReferenceStringAppend => Ok(reference_string_append()),
    }
}

fn account_create_drop() -> Result<Observation, OperationError> {
    let account = RequestAccount::try_new(limits())?;
    let final_usage = account.snapshot();
    drop(account);
    Ok(Observation::Account { final_usage })
}

fn work_cooperative_transitions() -> Result<Observation, OperationError> {
    let control = CountingControl::default();
    let mut context = black_box(context_with(&control, WORK_ENTRY_CREDITS))?;
    let mut route = Checksum::new();
    let mut entries = 1_u32;
    route.mix_u64(0);
    for index in 0..WORK_TRANSITIONS {
        if index > 0 && index % WORK_ENTRY_CREDITS == 0 {
            let accepted = black_box(context.try_begin_next_cooperative_entry(WORK_ENTRY_CREDITS))?;
            if !accepted {
                return Err(OperationError::UnexpectedEntryRejection);
            }
            route.mix_u64(u64::from(entries));
            entries += 1;
        }
        match black_box(context.admit_work_transition())? {
            WorkAdmission::Granted(amount) => {
                route.mix_u64(u64::from(index));
                route.mix_usize(black_box(amount));
            }
            WorkAdmission::Pending => return Err(OperationError::UnexpectedAdmission),
        }
    }
    let exhausted_credits = black_box(context.remaining_work());
    route.mix_u64(u64::from(exhausted_credits));
    let final_credits = exhausted_credits;
    context.check_control()?;
    let final_usage = context.snapshot();
    Ok(Observation::Work {
        final_usage,
        route: route.finish(),
        entries,
        checks: control.checks.get(),
        final_credits,
    })
}

fn nesting_enter_drop() -> Result<Observation, OperationError> {
    let context = context()?;
    let mut route = Checksum::new();
    for index in 0..NESTING_LIFECYCLES {
        let guard = black_box(context.enter_nesting())?;
        route.mix_u64(index);
        route.mix_u64(u64::from(black_box(context.snapshot().nesting_depth())));
        drop(guard);
        route.mix_u64(u64::from(black_box(context.snapshot().nesting_depth())));
    }
    let final_usage = context.snapshot();
    Ok(Observation::Nesting {
        final_usage,
        route: route.finish(),
    })
}

fn output_reserve_partial_commit() -> Result<Observation, OperationError> {
    let context = context()?;
    let mut route = Checksum::new();
    for index in 0..OUTPUT_RESERVATION_BATCH {
        let permit = black_box(context.reserve_output(OUTPUT_RESERVED_BYTES))?;
        let reserved = context.snapshot();
        route.mix_u64(index);
        route.mix_u64(black_box(permit.reserved_bytes()));
        route.mix_u64(black_box(reserved.output_bytes()));
        route.mix_u64(black_box(reserved.output_reserved_bytes()));
        permit.commit(OUTPUT_PUBLISHED_BYTES)?;
    }
    let final_usage = context.snapshot();
    Ok(Observation::OutputReservation {
        final_usage,
        route: route.finish(),
    })
}

fn reference_vec_push() -> Observation {
    let mut values = Vec::with_capacity(U64_VALUES);
    for &value in vector_values() {
        values.push(value);
    }
    let observation = Observation::ReferenceVec {
        len: values.len(),
        capacity: values.capacity(),
        samples: vector_samples(black_box(&values)),
    };
    drop(values);
    observation
}

fn reference_string_append() -> Observation {
    let mut value = String::with_capacity(STRING_BYTES);
    let chunk = string_chunk();
    for _ in 0..(STRING_BYTES / STRING_CHUNK_BYTES) {
        value.push_str(chunk);
    }
    let observation = Observation::ReferenceString {
        len: value.len(),
        capacity: value.capacity(),
        samples: string_samples(black_box(&value)),
    };
    drop(value);
    observation
}

fn validate_full_collection(kind: CaseKind) -> Result<Option<u64>, String> {
    let checksum = match kind {
        CaseKind::ReferenceVecPush => {
            let mut values = Vec::with_capacity(U64_VALUES);
            for &value in vector_values() {
                values.push(value);
            }
            ensure(
                values.as_slice() == vector_values(),
                "native vector contents differ from its fixture",
            )?;
            Some(checksum_u64(&values))
        }
        CaseKind::ReferenceStringAppend => {
            let mut value = String::with_capacity(STRING_BYTES);
            for _ in 0..(STRING_BYTES / STRING_CHUNK_BYTES) {
                value.push_str(string_chunk());
            }
            ensure(
                value == native_string(),
                "native string contents differ from its fixture",
            )?;
            Some(checksum_bytes(value.as_bytes()))
        }
        CaseKind::AccountCreateDrop
        | CaseKind::WorkCooperativeTransitions
        | CaseKind::NestingEnterDrop
        | CaseKind::OutputReservePartialCommit => None,
    };
    Ok(checksum)
}

fn vector_samples(values: &[u64]) -> VectorSamples {
    VectorSamples {
        first: values[0],
        middle: values[values.len() / 2],
        last: values[values.len() - 1],
    }
}

fn string_samples(value: &str) -> StringSamples {
    let bytes = value.as_bytes();
    StringSamples {
        first: bytes[0],
        one_third: bytes[bytes.len() / 3],
        two_thirds: bytes[bytes.len() * 2 / 3],
        last: bytes[bytes.len() - 1],
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one exhaustive dispatcher keeps case-to-receipt validation explicit"
)]
fn validate(kind: CaseKind, observation: &Observation) -> Result<(), String> {
    match (kind, observation) {
        (CaseKind::AccountCreateDrop, Observation::Account { final_usage }) => {
            validate_account(*final_usage, "account/create-drop")
        }
        (
            CaseKind::WorkCooperativeTransitions,
            Observation::Work {
                final_usage,
                route,
                entries,
                checks,
                final_credits,
            },
        ) => validate_work(*final_usage, *route, *entries, *checks, *final_credits),
        (CaseKind::NestingEnterDrop, Observation::Nesting { final_usage, route }) => {
            validate_nesting(*final_usage, *route)
        }
        (CaseKind::OutputReservePartialCommit, Observation::OutputReservation { final_usage, route }) => {
            validate_output_reservation(*final_usage, *route)
        }
        (CaseKind::ReferenceVecPush, Observation::ReferenceVec { len, capacity, samples }) => {
            validate_reference_vec(*len, *capacity, *samples)
        }
        (CaseKind::ReferenceStringAppend, Observation::ReferenceString { len, capacity, samples }) => {
            validate_reference_string(*len, *capacity, *samples)
        }
        _ => Err("benchmark case returned the wrong observation kind".to_owned()),
    }
}

fn validate_account(snapshot: UsageSnapshot, label: &str) -> Result<(), String> {
    let ledger = RequestAccount::minimum_memory_bytes();
    ensure_snapshot(snapshot, SnapshotExpectation::new(0, 0, ledger, ledger, 0, 0), label)
}

fn validate_work(
    snapshot: UsageSnapshot,
    route: u64,
    entries: u32,
    checks: u32,
    final_credits: u32,
) -> Result<(), String> {
    validate_account(snapshot, "work/cooperative-transitions")?;
    ensure(
        route == expected_work_route(),
        "work/cooperative-transitions route receipt changed",
    )?;
    ensure(
        entries == WORK_ENTRIES,
        "work/cooperative-transitions entry receipt changed",
    )?;
    ensure(
        checks == WORK_CONTROL_CHECKS,
        "work/cooperative-transitions control receipt changed",
    )?;
    ensure(
        final_credits == 0,
        "work/cooperative-transitions exhausted-credit receipt changed",
    )
}

fn validate_nesting(final_usage: UsageSnapshot, route: u64) -> Result<(), String> {
    let ledger = RequestAccount::minimum_memory_bytes();
    ensure_snapshot(
        final_usage,
        SnapshotExpectation::new(0, 0, ledger, ledger, 0, 1),
        "nesting/enter-drop final",
    )?;
    ensure(
        route == expected_nesting_route(),
        "nesting/enter-drop route receipt changed",
    )
}

fn validate_output_reservation(final_usage: UsageSnapshot, route: u64) -> Result<(), String> {
    let ledger = RequestAccount::minimum_memory_bytes();
    ensure_snapshot(
        final_usage,
        SnapshotExpectation::new(
            0,
            OUTPUT_RESERVATION_BATCH * OUTPUT_PUBLISHED_BYTES,
            ledger,
            ledger,
            0,
            0,
        ),
        "output/reserve-partial-commit final",
    )?;
    ensure(
        route == expected_output_route(),
        "output/reserve-partial-commit route receipt changed",
    )
}

fn validate_reference_vec(len: usize, capacity: usize, samples: VectorSamples) -> Result<(), String> {
    ensure(
        len == U64_VALUES && capacity == U64_VALUES,
        "native vector shape receipt changed",
    )?;
    ensure(
        samples == vector_samples(vector_values()),
        "native vector samples differ from its fixture",
    )
}

fn validate_reference_string(len: usize, capacity: usize, samples: StringSamples) -> Result<(), String> {
    ensure(
        len == STRING_BYTES && capacity == STRING_BYTES,
        "native string shape receipt changed",
    )?;
    ensure(
        samples == string_samples(native_string()),
        "native string samples differ from its fixture",
    )
}

fn expected_work_route() -> u64 {
    let mut route = Checksum::new();
    let mut entries = 1_u32;
    route.mix_u64(0);
    for index in 0..WORK_TRANSITIONS {
        if index > 0 && index % WORK_ENTRY_CREDITS == 0 {
            route.mix_u64(u64::from(entries));
            entries += 1;
        }
        route.mix_u64(u64::from(index));
        route.mix_usize(1);
    }
    route.mix_u64(0);
    route.finish()
}

fn expected_nesting_route() -> u64 {
    let mut route = Checksum::new();
    for index in 0..NESTING_LIFECYCLES {
        route.mix_u64(index);
        route.mix_u64(1);
        route.mix_u64(0);
    }
    route.finish()
}

fn expected_output_route() -> u64 {
    let mut route = Checksum::new();
    for index in 0..OUTPUT_RESERVATION_BATCH {
        route.mix_u64(index);
        route.mix_u64(OUTPUT_RESERVED_BYTES);
        route.mix_u64(index * OUTPUT_PUBLISHED_BYTES);
        route.mix_u64(OUTPUT_RESERVED_BYTES);
    }
    route.finish()
}

fn limits() -> ResourceLimits {
    ResourceLimits::new(1 << 26, 1 << 26, (STRING_BYTES as u64) * 4, 1 << 26, 512)
}

fn context() -> Result<ResourceContext<'static>, OperationError> {
    context_with(&CONTINUE_CONTROL, WORK_ENTRY_CREDITS)
}

fn context_with(control: &dyn Control, credits: u32) -> Result<ResourceContext<'_>, OperationError> {
    let account = RequestAccount::try_new(limits())?;
    let work = WorkMeter::try_new_v1(credits).ok_or(OperationError::InvalidFixedWorkCredits)?;
    Ok(ResourceContext::new(account, control, work)?)
}

#[derive(Clone, Copy)]
struct SnapshotExpectation {
    input: u64,
    output: u64,
    current: u64,
    peak: u64,
    nesting: u32,
    nesting_peak: u32,
    memory: [(u64, u64); 5],
}

impl SnapshotExpectation {
    #[allow(
        clippy::too_many_arguments,
        reason = "one fixed receipt keeps every public UsageSnapshot counter explicit at each call site"
    )]
    fn new(input: u64, output: u64, current: u64, peak: u64, nesting: u32, nesting_peak: u32) -> Self {
        Self {
            input,
            output,
            current,
            peak,
            nesting,
            nesting_peak,
            memory: [
                (
                    RequestAccount::minimum_memory_bytes(),
                    RequestAccount::minimum_memory_bytes(),
                ),
                (0, 0),
                (0, 0),
                (0, 0),
                (0, 0),
            ],
        }
    }
}

fn ensure_snapshot(snapshot: UsageSnapshot, expected: SnapshotExpectation, label: &str) -> Result<(), String> {
    if snapshot.input_bytes() != expected.input {
        return Err(snapshot_mismatch(label, "input usage"));
    }
    if snapshot.output_bytes() != expected.output {
        return Err(snapshot_mismatch(label, "output usage"));
    }
    if snapshot.output_reserved_bytes() != 0 {
        return Err(snapshot_mismatch(label, "reserved output usage"));
    }
    if snapshot.memory_current_bytes() != expected.current {
        return Err(snapshot_mismatch(label, "current memory"));
    }
    if snapshot.memory_peak_bytes() != expected.peak {
        return Err(snapshot_mismatch(label, "peak memory"));
    }
    if snapshot.nesting_depth() != expected.nesting {
        return Err(snapshot_mismatch(label, "nesting depth"));
    }
    if snapshot.nesting_peak() != expected.nesting_peak {
        return Err(snapshot_mismatch(label, "nesting peak"));
    }
    for (index, category) in categories().into_iter().enumerate() {
        let usage = snapshot.memory(category);
        let (current, peak) = expected.memory[index];
        if usage.current() != current {
            return Err(memory_snapshot_mismatch(label, category, "current"));
        }
        if usage.peak() != peak {
            return Err(memory_snapshot_mismatch(label, category, "peak"));
        }
    }
    Ok(())
}

fn snapshot_mismatch(label: &str, field: &str) -> String {
    format!("{label}: {field} receipt changed")
}

fn memory_snapshot_mismatch(label: &str, category: MemoryCategory, field: &str) -> String {
    format!("{label}: {category:?} {field} receipt changed")
}

const fn categories() -> [MemoryCategory; 4] {
    [
        MemoryCategory::Retained,
        MemoryCategory::Working,
        MemoryCategory::Diagnostic,
        MemoryCategory::PendingIo,
    ]
}

#[derive(Default)]
struct CountingControl {
    checks: Cell<u32>,
}

impl Control for CountingControl {
    fn check(&self) -> ControlOutcome {
        self.checks.set(self.checks.get() + 1);
        ControlOutcome::Continue
    }
}

fn vector_values() -> &'static [u64] {
    VECTOR_FIXTURE.get_or_init(|| {
        (0..U64_VALUES)
            .map(|index| (index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15))
            .collect()
    })
}

fn string_chunk() -> &'static str {
    STRING_CHUNK
        .get_or_init(|| {
            "jqf resource benchmark ".repeat(STRING_CHUNK_BYTES / 23) + &"x".repeat(STRING_CHUNK_BYTES % 23)
        })
        .as_str()
}

fn native_string() -> &'static str {
    STRING_FIXTURE
        .get_or_init(|| string_chunk().repeat(STRING_BYTES / STRING_CHUNK_BYTES))
        .as_str()
}

#[derive(Clone, Copy)]
struct Checksum(u64);

impl Checksum {
    const fn new() -> Self {
        Self(FNV_OFFSET)
    }

    fn mix_u64(&mut self, value: u64) {
        for byte in value.to_le_bytes() {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
        }
    }

    fn mix_usize(&mut self, value: usize) {
        self.mix_u64(value as u64);
    }

    fn mix_snapshot(&mut self, snapshot: UsageSnapshot) {
        self.mix_u64(snapshot.input_bytes());
        self.mix_u64(snapshot.output_bytes());
        self.mix_u64(snapshot.output_reserved_bytes());
        self.mix_u64(snapshot.memory_current_bytes());
        self.mix_u64(snapshot.memory_peak_bytes());
        self.mix_u64(u64::from(snapshot.nesting_depth()));
        self.mix_u64(u64::from(snapshot.nesting_peak()));
        for category in categories() {
            let usage = snapshot.memory(category);
            self.mix_u64(usage.current());
            self.mix_u64(usage.peak());
        }
    }

    fn mix_vector_samples(&mut self, samples: VectorSamples) {
        self.mix_u64(samples.first);
        self.mix_u64(samples.middle);
        self.mix_u64(samples.last);
    }

    fn mix_string_samples(&mut self, samples: StringSamples) {
        self.mix_u64(u64::from(samples.first));
        self.mix_u64(u64::from(samples.one_third));
        self.mix_u64(u64::from(samples.two_thirds));
        self.mix_u64(u64::from(samples.last));
    }

    const fn finish(self) -> u64 {
        self.0
    }
}

fn checksum_u64(values: &[u64]) -> u64 {
    let mut checksum = Checksum::new();
    for &value in values {
        checksum.mix_u64(value);
    }
    checksum.finish()
}

fn checksum_bytes(bytes: &[u8]) -> u64 {
    let mut checksum = Checksum::new();
    for &byte in bytes {
        checksum.0 = (checksum.0 ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
    }
    checksum.finish()
}

fn ensure(condition: bool, message: &str) -> Result<(), String> {
    condition.then_some(()).ok_or_else(|| message.to_owned())
}

const fn size_of<T>() -> usize {
    std::mem::size_of::<T>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "allocation-stats")]
    static ALLOCATION_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn every_case_has_a_stable_success_preflight() {
        for kind in CaseKind::ALL {
            let mut first = ResourceCase { kind };
            let mut second = ResourceCase { kind };
            assert_eq!(
                first.preflight().expect("first preflight succeeds"),
                second.preflight().expect("second preflight succeeds"),
                "{} receipt drifted",
                kind.metadata().name
            );
        }
    }

    #[test]
    fn measured_observations_validate_without_rerunning_the_operation() {
        for kind in CaseKind::ALL {
            let observation = run_operation(kind).expect("measured operation succeeds");
            validate(kind, &observation).expect("measured observation validates");
        }
    }

    #[test]
    fn metadata_matches_the_requested_batch_inventory() {
        let cases = cases();
        let metadata: Vec<_> = cases.iter().map(BenchmarkCase::metadata).collect();
        assert_eq!(metadata.len(), 6);
        assert_eq!(metadata[1].operations_per_invocation, u64::from(WORK_TRANSITIONS));
        assert_eq!(metadata[2].operations_per_invocation, NESTING_LIFECYCLES);
        assert_eq!(metadata[3].operations_per_invocation, OUTPUT_RESERVATION_BATCH);
        assert_eq!(
            metadata[3].bytes_per_invocation,
            OUTPUT_RESERVATION_BATCH * OUTPUT_PUBLISHED_BYTES
        );
        assert_eq!(metadata[4].operations_per_invocation, U64_VALUES as u64);
        assert_eq!(metadata[5].bytes_per_invocation, STRING_BYTES as u64);
    }

    #[cfg(feature = "allocation-stats")]
    #[test]
    fn measured_paths_exclude_receipt_and_guard_storage_allocations() {
        let _lock = ALLOCATION_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        vector_values();
        native_string();

        let account = allocation_stats(CaseKind::AccountCreateDrop);
        assert_eq!(account.allocation_calls, 1);
        assert_eq!(account.reallocation_calls, 0);

        let nesting = allocation_stats(CaseKind::NestingEnterDrop);
        assert_eq!(nesting.allocation_calls, 1);
        assert_eq!(nesting.reallocation_calls, 0);
        assert_eq!(
            nesting.requested_bytes,
            usize::try_from(RequestAccount::minimum_memory_bytes()).expect("ledger bytes fit usize")
        );

        let vector = allocation_stats(CaseKind::ReferenceVecPush);
        assert_eq!(vector.allocation_calls, 1);
        assert_eq!(vector.reallocation_calls, 0);
        assert_eq!(vector.requested_bytes, U64_VALUES * size_of::<u64>());

        let string = allocation_stats(CaseKind::ReferenceStringAppend);
        assert_eq!(string.allocation_calls, 1);
        assert_eq!(string.reallocation_calls, 0);
        assert_eq!(string.requested_bytes, STRING_BYTES);
    }

    #[cfg(feature = "allocation-stats")]
    #[test]
    fn collection_hot_witnesses_are_fixed_size_and_allocation_free() {
        let _lock = ALLOCATION_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let values = vector_values();
        let text = native_string();
        let ((vector, string), statistics) = jqf_bench_core::allocation::measure(|| {
            (vector_samples(black_box(values)), string_samples(black_box(text)))
        });
        assert_eq!(vector, vector_samples(values));
        assert_eq!(string, string_samples(text));
        assert_eq!(size_of::<VectorSamples>(), 3 * size_of::<u64>());
        assert_eq!(size_of::<StringSamples>(), 4);
        assert_eq!(statistics.allocation_calls, 0);
        assert_eq!(statistics.reallocation_calls, 0);
        assert_eq!(statistics.requested_bytes, 0);
        assert_eq!(statistics.peak_live_bytes, 0);
        assert_eq!(statistics.retained_bytes, 0);
    }

    #[cfg(feature = "allocation-stats")]
    fn allocation_stats(kind: CaseKind) -> jqf_bench_core::allocation::AllocationStats {
        let (_, statistics) = jqf_bench_core::allocation::measure(|| ResourceCase { kind }.run());
        statistics
    }

    #[test]
    fn optimizer_receipts_cover_every_fixed_loop_result() {
        let Observation::Work {
            route,
            entries,
            checks,
            final_credits,
            ..
        } = run_operation(CaseKind::WorkCooperativeTransitions).expect("transitions succeed")
        else {
            panic!("transitions returned the wrong observation");
        };
        assert_eq!(route, expected_work_route());
        assert_eq!(entries, WORK_ENTRIES);
        assert_eq!(checks, WORK_CONTROL_CHECKS);
        assert_eq!(final_credits, 0);

        let Observation::Nesting { route, .. } =
            run_operation(CaseKind::NestingEnterDrop).expect("nesting lifecycles succeed")
        else {
            panic!("nesting returned the wrong observation");
        };
        assert_eq!(route, expected_nesting_route());
    }
}
