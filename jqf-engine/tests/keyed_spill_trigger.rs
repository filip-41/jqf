//! The spill trigger's residency law: a keyed SORT collection over many
//! small-key records must ENGAGE the external sort under a modest budget.
//!
//! The trigger meters the encoded-key estimate PLUS one residency floor per
//! buffered entry ([`jqf_engine`] dispatch's `keyed_spill_meter`): one
//! numeric key estimates 24 bytes while the entry holding it costs several
//! times that, so the old key-only meter let a small-key sort finish without
//! ever spilling. These numbers are chosen to straddle the fix: 200 records
//! × 24-byte key estimate = 4,800 < the 8,192 budget, so a key-only meter
//! creates ZERO runs, while the entry floors push the metered total past it
//! well before the tail. Runs > 0 here fails on the pre-fix code.

use std::cell::RefCell;
use std::fmt::Write as _;

use jqf_codec_core::{DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
use jqf_data::{DialectId, FormatId};
use jqf_engine::{CodecRequirementPolicy, try_compile_program};
use jqf_resource::{
    ContinueControl, RequestAccount, ResourceContext, ResourceError, ResourceLimits, RunCursorId, RunId, SpillStore,
    WorkMeter,
};
use jqf_sdk::{CodecCatalog, Diagnostics, EncodedItemReport, FacadeFraming, ItemSink, PipelinePolicy};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

const COOPERATIVE_CREDITS: u32 = 64;
static CONTROL: ContinueControl = ContinueControl;

/// The spill budget: above the key-only estimate for the whole input
/// (200 × 24 = 4,800), below the metered total once the per-entry floors
/// join (200 × (24 + 64) = 17,600).
const SPILL_BUDGET: u64 = 8_192;
const ROWS: usize = 200;

/// An in-memory [`SpillStore`] that counts the runs it accepted. The run
/// format mirrors the writer's (`[u32 key_len][key bytes][u64 position]` per
/// entry) so the merge reads real entries back.
#[derive(Default)]
struct CountingStore {
    runs: RefCell<Vec<Vec<u8>>>,
    cursors: RefCell<Vec<(usize, usize)>>,
}

impl CountingStore {
    fn run_count(&self) -> usize {
        self.runs.borrow().len()
    }
}

impl SpillStore for CountingStore {
    fn create_run(&self) -> Result<RunId, ResourceError> {
        let mut runs = self.runs.borrow_mut();
        runs.push(Vec::new());
        let index = runs.len() - 1;
        Ok(RunId(index.try_into().map_err(|_| ResourceError::HostFailure {
            detail: "unreachable: in-memory store",
        })?))
    }

    fn write_run(&self, id: RunId, bytes: &[u8]) -> Result<(), ResourceError> {
        let index = usize::try_from(id.0).map_err(|_| ResourceError::HostFailure {
            detail: "unreachable: in-memory store",
        })?;
        self.runs.borrow_mut()[index] = bytes.to_vec();
        Ok(())
    }

    fn open_run(&self, id: RunId) -> Result<RunCursorId, ResourceError> {
        let run = usize::try_from(id.0).map_err(|_| ResourceError::HostFailure {
            detail: "unreachable: in-memory store",
        })?;
        let mut cursors = self.cursors.borrow_mut();
        cursors.push((run, 0));
        let cursor = cursors.len() - 1;
        Ok(RunCursorId(cursor.try_into().map_err(|_| {
            ResourceError::HostFailure {
                detail: "unreachable: in-memory store",
            }
        })?))
    }

    fn read_next(&self, cursor: RunCursorId, out: &mut Vec<u8>) -> Result<Option<u64>, ResourceError> {
        let cursor = usize::try_from(cursor.0).map_err(|_| ResourceError::HostFailure {
            detail: "unreachable: in-memory store",
        })?;
        let mut cursors = self.cursors.borrow_mut();
        let (run, offset) = &mut cursors[cursor];
        let run_bytes = &self.runs.borrow()[*run];
        if *offset >= run_bytes.len() {
            return Ok(None);
        }
        let len = u32::from_le_bytes(run_bytes[*offset..*offset + 4].try_into().expect("len")) as usize;
        out.extend_from_slice(&run_bytes[*offset + 4..*offset + 4 + len]);
        let position = u64::from_le_bytes(
            run_bytes[*offset + 4 + len..*offset + 12 + len]
                .try_into()
                .expect("position"),
        );
        *offset += 12 + len;
        Ok(Some(position))
    }

    fn delete_run(&self, _id: RunId) -> Result<(), ResourceError> {
        Ok(())
    }
}

struct CollectingSink {
    bytes: Vec<u8>,
}

impl CollectingSink {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }
}

impl ItemSink for CollectingSink {
    type Error = &'static str;

    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        Ok(())
    }
    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn finish_item(&mut self, _index: u64, _report: EncodedItemReport) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// `ROWS` objects with SMALL numeric keys in DESCENDING input order, so a
/// correct spilled sort publishes ascending keys.
fn spill_input() -> String {
    let mut input = String::from("[");
    for i in (0..ROWS).rev() {
        if i < ROWS - 1 {
            input.push(',');
        }
        write!(input, "{{\"k\":{i}}}").expect("write to a String");
    }
    input.push(']');
    input
}

/// Runs one program over the input with the given spill budget and the
/// counting store installed; returns (output bytes, store).
fn run_with_spill(program: &str, input: &str, budget: u64) -> (Vec<u8>, CountingStore) {
    let store = CountingStore::default();
    let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, budget, u32::MAX);
    let account = RequestAccount::try_new(limits).expect("account");
    let meter = WorkMeter::try_new_v1(COOPERATIVE_CREDITS).expect("work");
    let mut resources = ResourceContext::new(account, &CONTROL, meter)
        .expect("resources")
        .with_spill_store(&store);

    let registration = jqf_codec_json::registration().expect("json registration");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id");
    let compiled = try_compile_program(
        program,
        CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .expect("compile");
    let requirement = compiled.try_requirement(&resources).expect("requirement");
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "<test>",
        input.as_bytes(),
        0,
    );
    let mut sink = CollectingSink::new();
    let policy_options = PipelinePolicy {
        max_iterations: None,
        decode: DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: &dialect(),
            options: None,
            allow_adjacent_values: false,
            value_separator: jqf_codec_json::VALUE_SEPARATORS,
        },
        encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::Report,
        encode_options: None,
        cooperative_credits: COOPERATIVE_CREDITS,
        split: None,
    };
    let diagnostics = Diagnostics::new(DiagnosticPolicy::ErrorsOnly);
    let request = jqf_sdk::Request::new(&compiled, jqf_sdk::Input::Whole(input.as_bytes()))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(format(), dialect())
        .with_output_format(format(), dialect())
        .with_policy(policy_options)
        .with_framing(FacadeFraming::item_suffix(b"\n"))
        .with_resources(&mut resources)
        .with_diagnostics(diagnostics.as_ref())
        .with_requirement(&requirement);
    match jqf_sdk::execute(request, &mut sink) {
        Ok(_) => {}
        Err(error) => panic!("expected success for {program:?}, got: {:?}", error.pipeline_failure()),
    }
    (sink.bytes, store)
}

/// The published order is the ascending key order, first to last.
fn assert_ascending(output: &[u8]) {
    let flat: String = String::from_utf8(output.to_vec())
        .expect("utf-8")
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    let key_at = |key: usize| {
        flat.find(&format!("\"k\":{key}"))
            .unwrap_or_else(|| panic!("key {key} missing from output"))
    };
    assert!(key_at(0) < key_at(1), "first pair out of order: {flat}");
    assert!(
        key_at(0) < key_at(ROWS / 2) && key_at(ROWS / 2) < key_at(ROWS - 1),
        "mid/last keys out of order"
    );
}

/// The STATIC keyed lane (a bare `.k` key graph over a located input):
/// many small-key records cross the budget only through the entry floors.
#[test]
fn small_key_static_lane_spills_and_stays_sorted() {
    let input = spill_input();
    let (output, store) = run_with_spill("sort_by(.k)", &input, SPILL_BUDGET);
    assert!(store.run_count() > 0, "the spill never engaged");
    assert_ascending(&output);
}

/// The GENERAL keyed drive (a construct-array key graph): same law.
#[test]
fn small_key_general_drive_spills_and_stays_sorted() {
    let input = spill_input();
    let (output, store) = run_with_spill("sort_by([.k])", &input, SPILL_BUDGET);
    assert!(store.run_count() > 0, "the spill never engaged");
    assert_ascending(&output);
}

/// `ROWS` records whose keys are scalar EXCEPT two object keys: one past the
/// first flush point (~entry 93 under the entry-floor meter) so an early run
/// writes successfully, one at the end. The chunks holding the objects
/// decline the whole spill (the closed table), so the TAIL flush declines
/// too — the recovery must fold the runs back into the in-memory sort and
/// publish the never-spilled answer, not raise an internal contract.
fn mixed_key_input() -> String {
    let mut input = String::from("[");
    for i in 0..ROWS {
        if i > 0 {
            input.push(',');
        }
        if i == 150 || i == ROWS - 1 {
            write!(input, "{{\"a\":{{\"x\":{i}}}}}").expect("write to a String");
        } else {
            write!(input, "{{\"a\":{i}}}").expect("write to a String");
        }
    }
    input.push(']');
    input
}

fn assert_mixed_keys_match_the_unspilled_answer(program: &str) {
    let input = mixed_key_input();
    let (spilled, store) = run_with_spill(program, &input, SPILL_BUDGET);
    assert!(store.run_count() > 0, "the spill never engaged");
    let (in_memory, plain_store) = run_with_spill(program, &input, u64::MAX);
    assert_eq!(plain_store.run_count(), 0, "the control spilled");
    assert_eq!(spilled, in_memory, "recovered sort diverged from the floor");
}

/// The STATIC keyed lane's tail: a declined final flush must recover.
#[test]
fn mixed_keys_static_lane_recovers_from_a_declined_tail_flush() {
    assert_mixed_keys_match_the_unspilled_answer("sort_by(.a)");
}

/// The GENERAL keyed drive's tail: same law.
#[test]
fn mixed_keys_general_drive_recovers_from_a_declined_tail_flush() {
    assert_mixed_keys_match_the_unspilled_answer("sort_by([.a])");
}
