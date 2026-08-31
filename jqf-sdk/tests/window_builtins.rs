//! The window/rolling builtin family (`EXTENSION_PRELUDE`).
//!
//! The the parity gates, through the public SDK surface: every builtin's output
//! is byte-identical to its hand-written `foreach` expansion over a pinned
//! fixture (parity), the generator-parameterized shape composes with `inputs`
//! (streaming), and the names ride `--list-builtins`' enumeration source
//! (`builtins/0`). The family is reference-source prelude — no engine work — so the
//! parity gate is what pins each spelling.

use jqf_codec_core::{DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
use jqf_data::{DialectId, FormatId};
use jqf_engine::{CodecRequirementPolicy, CompileOptions, try_compile_program};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_sdk::{
    CodecCatalog, EncodedItemReport, FacadeFraming, Input, ItemSink, Outcome, PipelinePolicy, Report, Request,
};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

const COOPERATIVE_CREDITS: u32 = 64;
static CONTROL: ContinueControl = ContinueControl;

struct CollectingSink {
    bytes: Vec<u8>,
    items: usize,
}

impl CollectingSink {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            items: 0,
        }
    }
}

impl ItemSink for CollectingSink {
    type Error = &'static str;

    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        self.items += 1;
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
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 64 << 20, 0, 128)).expect("account allocates"),
        &CONTROL,
        WorkMeter::try_new_v1(COOPERATIVE_CREDITS).expect("work meter starts"),
    )
    .expect("resources start")
}

fn policy() -> PipelinePolicy<'static> {
    let options: &'static jqf_codec_json::JsonEncodeOptions = Box::leak(Box::new(jqf_codec_json::JsonEncodeOptions {
        indent: jqf_codec_json::JsonIndent::Compact,
        raw_strings: false,
        sort_keys: false,
        ascii_output: false,
        raw_output_nul: false,
    }));
    let dialect: &'static DialectId = Box::leak(Box::new(DialectId::try_new("rfc8259").expect("dialect")));
    PipelinePolicy {
        decode: DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect,
            options: None,
            allow_adjacent_values: true,
            value_separator: jqf_codec_json::VALUE_SEPARATORS,
        },
        encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        encode_options: Some(options as &(dyn core::any::Any + Send + Sync)),
        cooperative_credits: COOPERATIVE_CREDITS,
        split: None,

        max_iterations: None,
    }
}

/// Runs one JSON adjacent-value sequence and returns the compact bytes.
fn run_sequence(input: &[u8], program: &str) -> Vec<u8> {
    run_drive(input, program, false)
}

/// Runs the `-n` (null-first) drive, which seeds the shared input cursor so an
/// input-family program (`inputs`) drains the source; `execute_sequence` does
/// not serve the input family.
fn run_null_first(input: &[u8], program: &str) -> Vec<u8> {
    run_drive(input, program, true)
}

fn run_drive(input: &[u8], program: &str, null_first: bool) -> Vec<u8> {
    let registration = jqf_codec_json::registration().expect("json registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid");
    let mut resources = resources();
    let requirement_policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled =
        try_compile_program(program, requirement_policy, CompileOptions::new(), &resources).expect("program compiles");
    let requirement = compiled.try_requirement(&resources).expect("requirement lowers");
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.json",
        input,
        0,
    );
    let mut sink = CollectingSink::new();
    let mut request = Request::new(&compiled, Input::Whole(input))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(format(), dialect())
        .with_output_format(format(), dialect())
        .with_policy(policy())
        .with_framing(FacadeFraming::item_suffix(b"\n"))
        .with_resources(&mut resources)
        .with_requirement(&requirement);
    if null_first {
        request = request.with_null_input();
    }
    let outcome = jqf_sdk::execute(request, &mut sink).expect("sequence completes");
    match outcome {
        Outcome::Served(Report::Sequence(_)) => {}
        Outcome::Served(other) => panic!("unexpected drive report: {other:?}"),
        Outcome::Declined => panic!("the sequence drive must not decline"),
    }
    sink.bytes
}

/// The window/rolling fixture: a 1-based sawtooth with a negative run, chosen
/// to exercise window fill, window slide, and the first-value seeds of
/// `ewma`/`deltas`/`lag` in one pass.
const FIXTURE: &str = r#"{"v":[1,2,3,4,5,-1,-2,-3]}"#;

/// The parity gate: each builtin is byte-identical to its hand-written
/// `foreach` expansion over the pinned fixture.
#[test]
fn every_builtin_matches_its_hand_written_foreach_expansion() {
    let builtin = run_sequence(FIXTURE.as_bytes(), "[windows(3; .v[])]");
    let hand = run_sequence(FIXTURE.as_bytes(), "[foreach .v[] as $x ([]; (. + [$x])[-3:])]");
    assert_eq!(builtin, hand, "windows/2 vs its foreach expansion");
    assert_eq!(
        String::from_utf8(builtin).unwrap(),
        "[[1],[1,2],[1,2,3],[2,3,4],[3,4,5],[4,5,-1],[5,-1,-2],[-1,-2,-3]]\n"
    );

    let builtin = run_sequence(FIXTURE.as_bytes(), "[moving_sum(3; .v[])]");
    let hand = run_sequence(
        FIXTURE.as_bytes(),
        "[foreach .v[] as $x ({q: [], s: 0}; .q += [$x] | .s += $x | if (.q|length) > 3 then .s -= .q[0] | .q |= .[1:] else . end; .s)]",
    );
    assert_eq!(builtin, hand, "moving_sum/2 vs its foreach expansion");

    let builtin = run_sequence(FIXTURE.as_bytes(), "[moving_avg(3; .v[])]");
    let hand = run_sequence(
        FIXTURE.as_bytes(),
        "[foreach .v[] as $x ({q: [], s: 0}; .q += [$x] | .s += $x | if (.q|length) > 3 then .s -= .q[0] | .q |= .[1:] else . end; .s / (.q|length))]",
    );
    assert_eq!(builtin, hand, "moving_avg/2 vs its foreach expansion");

    let builtin = run_sequence(FIXTURE.as_bytes(), "[moving_min(3; .v[])]");
    let hand = run_sequence(FIXTURE.as_bytes(), "[foreach .v[] as $x ([]; (. + [$x])[-3:]; min)]");
    assert_eq!(builtin, hand, "moving_min/2 vs its foreach expansion");

    let builtin = run_sequence(FIXTURE.as_bytes(), "[moving_max(3; .v[])]");
    let hand = run_sequence(FIXTURE.as_bytes(), "[foreach .v[] as $x ([]; (. + [$x])[-3:]; max)]");
    assert_eq!(builtin, hand, "moving_max/2 vs its foreach expansion");

    let builtin = run_sequence(FIXTURE.as_bytes(), "[ewma(0.5; .v[])]");
    let hand = run_sequence(
        FIXTURE.as_bytes(),
        "[foreach .v[] as $x (null; if . == null then $x else 0.5 * $x + (1 - 0.5) * . end)]",
    );
    assert_eq!(builtin, hand, "ewma/2 vs its foreach expansion");

    let builtin = run_sequence(FIXTURE.as_bytes(), "[deltas(.v[])]");
    let hand = run_sequence(
        FIXTURE.as_bytes(),
        "[foreach .v[] as $x ({p: null, first: true}; {p: $x} + (if .first then {first: false, skip: true} else {first: false, d: ($x - .p)} end); if .skip then empty else .d end)]",
    );
    assert_eq!(builtin, hand, "deltas/1 vs its foreach expansion");
    assert_eq!(String::from_utf8(builtin).unwrap(), "[1,1,1,1,-6,-1,-1]\n");

    let builtin = run_sequence(FIXTURE.as_bytes(), "[lag(.v[])]");
    let hand = run_sequence(
        FIXTURE.as_bytes(),
        "[foreach .v[] as $x ({p: null, first: true}; {p: $x, first: false} + (if .first then {skip: true} else {v: .p} end); if .skip then empty else .v end)]",
    );
    assert_eq!(builtin, hand, "lag/1 vs its foreach expansion");

    let builtin = run_sequence(FIXTURE.as_bytes(), "[counter(.v[])]");
    let hand = run_sequence(FIXTURE.as_bytes(), "[foreach .v[] as $x (0; . + 1)]");
    assert_eq!(builtin, hand, "counter/1 vs its foreach expansion");
    assert_eq!(String::from_utf8(builtin).unwrap(), "[1,2,3,4,5,6,7,8]\n");

    // `running(f; g)` is its own expansion by construction (the plan's sketch);
    // `f` sees the state as `.` — the reference's call-by-name law — so a state-only scan
    // is the honest spelling.
    let builtin = run_sequence(
        FIXTURE.as_bytes(),
        "[running(if . == null then 0 else . + 1 end; .v[])]",
    );
    let hand = run_sequence(
        FIXTURE.as_bytes(),
        "[foreach .v[] as $x (null; if . == null then 0 else . + 1 end)]",
    );
    assert_eq!(builtin, hand, "running/2 vs its foreach expansion");
}

/// The family composes with the record-lane generator shape (`inputs`), the
/// `limit($n; g)`-style argument order, and per-record streaming.
#[test]
fn generator_parameterized_shape_streams_over_inputs() {
    let builtin = run_null_first(
        b"{\"v\":1}\n{\"v\":2}\n{\"v\":3}\n{\"v\":4}\n",
        "[moving_avg(2; inputs|.v)]",
    );
    assert_eq!(String::from_utf8(builtin).unwrap(), "[1,1.5,2.5,3.5]\n");
}

/// Every family name is registered in the enumeration `builtins/0` and
/// `--list-builtins` both read.
#[test]
fn the_family_rides_the_builtins_enumeration() {
    let output = run_sequence(b"null\n", "builtins");
    let text = String::from_utf8(output).unwrap();
    for entry in [
        "windows/2",
        "moving_sum/2",
        "moving_avg/2",
        "moving_min/2",
        "moving_max/2",
        "ewma/2",
        "deltas/1",
        "lag/1",
        "running/2",
        "counter/1",
    ] {
        assert!(text.contains(entry), "builtins/0 must list {entry}");
    }
}
