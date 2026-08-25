//! The streaming aggregate drive (`[inputs] | group_by/min_by/max_by`):
//! byte identity against the collect-then-key path it replaces, and the
//! ledger-based attribution of where retention goes on each path.
//!
//! The fusion is a RUNTIME drive change below classification: the compiled
//! graph is identical either way, so projection classes and route selection
//! are unchanged by construction. Every row here pins the other half of that
//! claim — the published bytes — plus the ledger split this lane exists to
//! attribute: subject-array-plus-entry-table retention on
//! the materialized path versus per-key accumulator retention on the fused
//! one.

use jqf_codec_core::{DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
use jqf_codec_json::ndjson::{NdjsonDecodeOptions, NdjsonProfile};
use jqf_data::{DialectId, FormatId};
use jqf_engine::{CodecRequirementPolicy, try_compile_program};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_sdk::{
    CodecCatalog, EncodedItemReport, FacadeFraming, Input, ItemSink, Outcome, PipelinePolicy, Report, Request,
};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

const COOPERATIVE_CREDITS: u32 = 64;
const INPUT_CEILING: u64 = 32 << 20;
static CONTROL: ContinueControl = ContinueControl;

/// The counting allocator over the system allocator, so `install()` +
/// `snapshot()` see every byte this test binary allocates. The CLI wires the
/// same wrapper around mimalloc; a test needs the accounting, not the speed.
#[global_allocator]
static LEDGER_ALLOC: jqf_resource::CountingAlloc<std::alloc::System> = jqf_resource::CountingAlloc(std::alloc::System);

fn json_dialect() -> &'static DialectId {
    Box::leak(Box::new(DialectId::try_new("rfc8259").expect("dialect")))
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

fn resources() -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 256 << 20, 0, 128)).expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(COOPERATIVE_CREDITS).expect("work"),
    )
    .expect("resources")
}

/// One `-n` record run: published bytes plus the LEDGER PEAK, which is the
/// attribution instrument — exact charged bytes, not wall time or RSS.
struct StreamRun {
    result: Result<Vec<u8>, String>,
    peak_ledger_bytes: u64,
}

fn run_null_first(input: &[u8], program: &str) -> StreamRun {
    let json = jqf_codec_json::registration().expect("json registration");
    let streams = jqf_codec_json::ndjson::registration().expect("ndjson registration");
    let registrations = [&json, &streams];
    let catalog = CodecCatalog::new(&registrations);
    let format = FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect");
    let mut resources = resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled = try_compile_program(program, policy, &resources).expect("compiles");
    let requirement = compiled
        .try_pulled_record_requirement(&resources)
        .expect("pulled-record requirement lowers");
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.ndjson",
        input,
        0,
    );
    let record_options = NdjsonDecodeOptions::try_new(None, INPUT_CEILING).expect("record ceiling");
    let records = jqf_codec_json::ndjson::create_record_provider(
        source,
        NdjsonProfile::Strict,
        record_options,
        DiagnosticPolicy::ErrorsOnly,
        ValidationMode::Strict,
        &mut resources,
    )
    .expect("record provider");
    let mut sink = CollectingSink::new();
    // Install the account as THIS thread's ambient ledger, exactly what the
    // CLI does for a request, so every heap allocation the drive makes lands
    // in the cells `snapshot()` reads. Without it the counters stay at their
    // construction baseline and any comparison between runs is vacuous.
    let ledger = jqf_resource::install(resources.account().try_share().expect("share request account"));
    let request = Request::new(
        &compiled,
        Input::Records {
            source: source.bytes(),
            records,
            slot: jqf_codec_json::ndjson::RECORD_ROUTE_SLOT,
        },
    )
    .with_catalog(catalog)
    .with_source(source)
    .with_format(format.clone(), dialect.clone())
    .with_output_format(format, dialect)
    .with_policy(PipelinePolicy {
        max_iterations: None,
        decode: DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: json_dialect(),
            options: None,
            allow_adjacent_values: false,
            value_separator: jqf_codec_json::VALUE_SEPARATORS,
        },
        encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        encode_options: None,
        cooperative_credits: COOPERATIVE_CREDITS,
        split: None,
    })
    .with_framing(FacadeFraming::item_suffix(b"\n"))
    .with_resources(&mut resources)
    .with_requirement(&requirement)
    .with_null_input();
    let outcome = jqf_sdk::execute(request, &mut sink);
    // Read the ledger while the account is still installed: the peak covers
    // the whole request.
    let peak_ledger_bytes = resources.snapshot().memory_peak_bytes();
    drop(ledger);
    let result = match outcome {
        Ok(Outcome::Served(Report::Record(_))) => Ok(sink.bytes),
        Ok(other) => Err(format!("unexpected outcome: {other:?}")),
        Err(error) => Err(format!(
            "execute failed: {}",
            error
                .pipeline_failure()
                .map_or_else(|| error.to_string(), ToString::to_string)
        )),
    };
    StreamRun {
        result,
        peak_ledger_bytes,
    }
}

/// The fused spelling and its collect-then-key twin must publish IDENTICAL
/// bytes and fail identically, whatever the records hold.
fn assert_byte_identity(input: &[u8], fused: &str, unfused: &str) {
    let fused_run = run_null_first(input, fused);
    let bound_run = run_null_first(input, unfused);
    match (&fused_run.result, &bound_run.result) {
        (Ok(fused_bytes), Ok(bound_bytes)) => {
            assert_eq!(fused_bytes, bound_bytes, "{fused} vs {unfused}");
        }
        (Err(_), Err(_)) => {}
        (fused_result, bound_result) => {
            panic!("completion diverged for {fused} vs {unfused}: {fused_result:?} vs {bound_result:?}")
        }
    }
}

#[test]
fn streamed_group_by_publishes_the_collect_path_bytes() {
    let input = b"{\"v\":1,\"k\":\"b\"}\n{\"v\":2,\"k\":\"a\"}\n{\"v\":3,\"k\":\"b\"}\n";
    assert_byte_identity(input, "[inputs] | group_by(.k)", "[inputs] as $r | $r | group_by(.k)");
    let run = run_null_first(input, "[inputs] | group_by(.k)");
    // Groups in key order, members stable within each group: the identity
    // assert above pins the bytes against the collect path; this row keeps a
    // positive shape witness so a both-sides-empty regression cannot slip by.
    let bytes = run.result.expect("run");
    let text = core::str::from_utf8(&bytes).expect("utf-8");
    assert!(
        text.contains("\"v\":2") && text.contains("\"v\":1") && text.contains("\"v\":3"),
        "{text}"
    );
}

#[test]
fn streamed_group_by_nan_keys_stay_distinct_groups_below_numbers() {
    let input = b"{\"v\":1,\"k\":NaN}\n{\"v\":2,\"k\":5}\n{\"v\":3,\"k\":NaN}\n{\"v\":4,\"k\":1}\n";
    assert_byte_identity(input, "[inputs] | group_by(.k)", "[inputs] as $r | $r | group_by(.k)");
    // Two singleton NaN groups in first-seen order, then the numbers ascending;
    // NaN renders as the null literal (the render law), the grouping is what
    // this row pins.
    let run = run_null_first(input, "[inputs] | group_by(.k)");
    let bytes = run.result.expect("run");
    let text = core::str::from_utf8(&bytes).expect("utf-8");
    assert_eq!(text.matches("\"v\":1").count(), 1);
    assert_eq!(text.matches("\"v\":3").count(), 1);
    let first = text.find("\"v\":1").expect("first nan group");
    let second = text.find("\"v\":3").expect("second nan group");
    let five = text.find("\"v\":2").expect("key 5");
    let one = text.find("\"v\":4").expect("key 1");
    assert!(first < second && second < one && one < five, "{text}");
}

#[test]
fn streamed_group_by_duplicate_and_negative_zero_keys_merge_in_input_order() {
    let input = b"{\"v\":1,\"k\":0}\n{\"v\":2,\"k\":-0}\n{\"v\":3,\"k\":0}\n";
    assert_byte_identity(input, "[inputs] | group_by(.k)", "[inputs] as $r | $r | group_by(.k)");
    let run = run_null_first(input, "[inputs] | group_by(.k)");
    let zero_text_bytes = run.result.expect("run");
    let text = core::str::from_utf8(&zero_text_bytes).expect("utf-8");
    // ONE group (-0 == 0 under both laws), members in input order.
    assert_eq!(text.matches("[{").count(), 1, "{text}");
    let v1 = text.find("\"v\":1").expect("first");
    let v2 = text.find("\"v\":2").expect("second");
    let v3 = text.find("\"v\":3").expect("third");
    assert!(v1 < v2 && v2 < v3, "{text}");
}

#[test]
fn streamed_group_by_multi_output_keys_and_single_element_and_empty_input() {
    let multi = b"{\"a\":1,\"b\":2,\"v\":9}\n{\"a\":1,\"b\":1,\"v\":8}\n{\"a\":1,\"b\":2,\"v\":7}\n";
    assert_byte_identity(
        multi,
        "[inputs] | group_by(.a,.b)",
        "[inputs] as $r | $r | group_by(.a,.b)",
    );
    let single = b"{\"v\":1,\"k\":\"x\"}\n";
    assert_byte_identity(single, "[inputs] | group_by(.k)", "[inputs] as $r | $r | group_by(.k)");
    // Empty stream: the empty law ([] for group_by, null for min/max), both
    // spellings.
    assert_byte_identity(b"", "[inputs] | group_by(.k)", "[inputs] as $r | $r | group_by(.k)");
    assert_byte_identity(b"", "[inputs] | min_by(.k)", "[inputs] as $r | $r | min_by(.k)");
    assert_eq!(
        run_null_first(b"", "[inputs] | group_by(.k)").result.expect("run"),
        b"[]\n"
    );
    assert_eq!(
        run_null_first(b"", "[inputs] | min_by(.k)").result.expect("run"),
        b"null\n"
    );
}

#[test]
fn streamed_min_max_keep_the_extreme_tie_laws_over_records() {
    let plain = b"{\"v\":1}\n{\"v\":2}\n{\"v\":3}\n";
    for (program, expected) in [
        ("[inputs] | min_by(.v)", "{\"v\":1}"),
        ("[inputs] | max_by(.v)", "{\"v\":3}"),
    ] {
        let fused = run_null_first(plain, program).result.expect("run");
        let text = core::str::from_utf8(&fused).expect("utf-8");
        assert!(text.trim_end().ends_with(expected), "{program}: {text}");
        assert_byte_identity(plain, program, &format!("[inputs] as $r | $r | {}", &program[10..]));
    }
    // NaN ties: min_by keeps the LAST NaN (observable Less replaces),
    // max_by skips NaN keys entirely (they sit below every number).
    let nans = b"{\"v\":1,\"k\":NaN}\n{\"v\":2,\"k\":5}\n{\"v\":3,\"k\":NaN}\n";
    assert_byte_identity(nans, "[inputs] | min_by(.k)", "[inputs] as $r | $r | min_by(.k)");
    assert_byte_identity(nans, "[inputs] | max_by(.k)", "[inputs] as $r | $r | max_by(.k)");
    let min_run = run_null_first(nans, "[inputs] | min_by(.k)").result.expect("min");
    let min_text = core::str::from_utf8(&min_run).expect("utf-8");
    assert!(min_text.contains("\"v\":3"), "{min_text}");
    let max_run = run_null_first(nans, "[inputs] | max_by(.k)").result.expect("max");
    let max_text = core::str::from_utf8(&max_run).expect("utf-8");
    assert!(max_text.contains("\"v\":2"), "{max_text}");
}

#[test]
fn streamed_aggregate_errors_match_the_collect_path() {
    // A malformed record mid-stream fails both spellings at the pull site.
    let broken = b"{\"v\":1,\"k\":\"a\"}\nnot-json\n{\"v\":2,\"k\":\"b\"}\n";
    assert_byte_identity(broken, "[inputs] | group_by(.k)", "[inputs] as $r | $r | group_by(.k)");
    // A key filter raise fails both spellings identically.
    let strings = b"{\"k\":\"s\"}\n";
    assert_byte_identity(
        strings,
        "[inputs] | group_by(.k.v)",
        "[inputs] as $r | $r | group_by(.k.v)",
    );
}

/// G0-lite attribution: WHERE retention goes, in exact ledger bytes.
///
/// The collect-then-key path books three terms per element — the decoded
/// value (retained by the subject array), its entry in the N-row key table,
/// and the published output shell. The streamed drive removes the middle
/// term's STRUCTURE (no subject array, no N-entry table): each element is
/// filed once into its key's accumulator. Because `group_by`'s OUTPUT must
/// contain every element, the decoded values stay resident under BOTH paths
/// — the honest win for bare `group_by` is bounded by the entry-table and
/// array-shell terms, which these assertions pin as the no-double-hold law;
/// the SELECTION modes have no such floor, and their fused peak collapses to
/// the streaming floor (one winner instead of every element).
#[test]
fn attribution_the_streamed_drive_removes_the_subject_and_entry_retention() {
    const RECORDS: usize = 20_000;
    let mut input = Vec::new();
    for index in 0..RECORDS {
        input.extend_from_slice(
            format!(
                "{{\"v\":{},\"k\":\"k{}\",\"pad\":\"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"}}\n",
                index,
                index % 12
            )
            .as_bytes(),
        );
    }
    // The retention FLOOR: every decoded element resident exactly once, no
    // keyed machinery (the collect array of the plain hold-everything shape).
    let hold_floor = run_null_first(&input, "[inputs] as $r | $r | length");
    let fused = run_null_first(&input, "[inputs] | group_by(.k) | length");
    let bound = run_null_first(&input, "[inputs] as $r | $r | group_by(.k) | length");
    assert_eq!(fused.result.expect("fused run"), bound.result.expect("bound run"));
    // No DOUBLE materialization on the streamed shape: the fused drive files
    // each element into ONE accumulator, so the whole request stays under 1.5x
    // the single-hold floor (elements once, plus bounded keyed bookkeeping and
    // the publication shells). A materialized subject held BESIDE per-key
    // accumulators would push the ratio past 2x on the element bytes alone.
    assert!(
        fused.peak_ledger_bytes * 2 < hold_floor.peak_ledger_bytes * 3,
        "fused group_by peak {} must stay within 1.5x the single-hold floor {}",
        fused.peak_ledger_bytes,
        hold_floor.peak_ledger_bytes
    );
    // And it never regresses past the collect path, whose per-element
    // bookkeeping (entry structs beside the subject array) is a superset of
    // one accumulator slot. The margin here is small BY LAW — the output must
    // contain every element either way — which is what makes the selection
    // rows below the sharp end of this lane.
    assert!(
        fused.peak_ledger_bytes < bound.peak_ledger_bytes * 105 / 100,
        "fused group_by peak {} regressed past the collect path's {}",
        fused.peak_ledger_bytes,
        bound.peak_ledger_bytes
    );
    // Selection modes: O(1) retention. One winner replaces every element, so
    // the fused peak collapses toward the streaming floor while the collect
    // path still holds every decoded record.
    let fused_max = run_null_first(&input, "[inputs] | max_by(.v)");
    let bound_max = run_null_first(&input, "[inputs] as $r | $r | max_by(.v)");
    assert_eq!(
        fused_max.result.expect("fused max run"),
        bound_max.result.expect("bound max run")
    );
    assert!(
        fused_max.peak_ledger_bytes * 4 < bound_max.peak_ledger_bytes,
        "fused max_by peak {} must be well under a quarter of the collect          path's {}: the winner should be all it retains",
        fused_max.peak_ledger_bytes,
        bound_max.peak_ledger_bytes
    );
}
