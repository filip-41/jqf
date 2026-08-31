//! The Gate A rung — the BARE-SLICE publish — end to end
//! through the public SDK surface with the real JSON codec.
//!
//! `tools/jqf-cli-jq-compat.sh` owns the byte oracle against jq and
//! `tools/smoke/jqf-sdk-smoke`'s `force_route_corpus` receipt owns the route-vs-floor
//! comparison over the shape space. These tests pin the laws neither of those
//! can observe from outside: the ledger stays proportional to the RANGE rather
//! than to the container (the whole point of the rung), the cooperative credit
//! quantum does not change the bytes, every arm of the container
//! dispatch declines with NOTHING published, and an adjacent-value input declines
//! so the sequence path keeps owning continue-on-error.

fn json_dialect() -> &'static DialectId {
    Box::leak(Box::new(DialectId::try_new("rfc8259").expect("dialect")))
}

use jqf_codec_core::{DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
use jqf_data::{DialectId, FormatId};
use jqf_engine::{CodecRequirementPolicy, CompileOptions, try_compile_program};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_sdk::{
    CodecCatalog, EncodedItemReport, FacadeFraming, Input, ItemSink, Outcome, PipelinePolicy, PublicationStatus,
    Report, Request,
};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

/// Local mirror of the drive-level range-locate outcome: the drive type is
/// crate-private, so tests reconstruct it from `Outcome` exactly as
/// request.rs does.
#[derive(Debug)]
// The mirror exists so `matches!` arms keep their names; not every variant
// is constructed or read by every suite.
#[allow(dead_code)]
enum RangeLocateRun {
    Completed(jqf_sdk::PipelineReport),
    NotSingleDocument,
    NotApplicable,
}

/// Far past the codec's sealed run length, so a container-proportional cost
/// would be visible in the memory ceiling below.
const MANY_ELEMENTS: usize = 20_000;

static CONTROL: ContinueControl = ContinueControl;

struct CollectingSink {
    bytes: Vec<u8>,
    items: usize,
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

fn resources(memory_bytes: u64, credits: u32) -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, memory_bytes, 0, 128))
            .expect("account allocates"),
        &CONTROL,
        WorkMeter::try_new_v1(credits).expect("work meter starts"),
    )
    .expect("resources start")
}

/// A single document whose `.catalog` holds `elements` records.
fn catalog_document(elements: usize) -> Vec<u8> {
    let mut bytes = Vec::from(br#"{"catalog":["#.as_slice());
    for index in 0..elements {
        if index > 0 {
            bytes.push(b',');
        }
        bytes.extend_from_slice(format!(r#"{{"id":{index},"name":"item-{index}"}}"#).as_bytes());
    }
    bytes.extend_from_slice(br"]}");
    bytes
}

/// Drives one Gate-A-eligible program through the range-locate route.
fn run_locate(
    input: &[u8],
    program_source: &str,
    memory_bytes: u64,
    credits: u32,
    allow_adjacent_values: bool,
) -> (Result<RangeLocateRun, jqf_sdk::PipelineError<String>>, CollectingSink) {
    let registration = jqf_codec_json::registration().expect("json registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid");
    let mut resources = resources(memory_bytes, credits);
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled =
        try_compile_program(program_source, policy, CompileOptions::new(), &resources).expect("program compiles");
    assert!(
        compiled.range_locate_eligible(),
        "{program_source} must be range-locate eligible"
    );
    let requirement = compiled
        .try_range_locate_requirement(&resources)
        .expect("range-locate requirement lowers");
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "slice.json",
        input,
        0,
    );
    let mut sink = CollectingSink {
        bytes: Vec::new(),
        items: 0,
    };
    let request = Request::new(&compiled, Input::Whole(input))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(format(), dialect())
        .with_output_format(format(), dialect())
        .with_policy({
            let dialect = json_dialect();
            PipelinePolicy {
                decode: DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect,
                    options: None,
                    allow_adjacent_values,
                    value_separator: jqf_codec_json::VALUE_SEPARATORS,
                },
                encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
                preservation: PreservationRequest::None,
                encode_options: None,
                cooperative_credits: credits,
                split: None,

                max_iterations: None,
            }
        })
        .with_framing(FacadeFraming::item_suffix(b"\n"))
        .with_resources(&mut resources)
        .with_requirement(&requirement)
        .range_locate();
    let run = match jqf_sdk::execute(request, &mut sink) {
        Ok(Outcome::Served(Report::Pipeline(report))) => RangeLocateRun::Completed(report),
        Ok(Outcome::Served(other)) => panic!("unexpected drive report: {other:?}"),
        Ok(Outcome::Declined) => RangeLocateRun::NotApplicable,
        Err(error) => match error {
            jqf_sdk::Failure::Pipeline(error) => return (Err(error), sink),
            other => panic!("unexpected failure class: {other:?}"),
        },
    };
    (Ok(run), sink)
}

#[test]
fn a_ten_element_range_of_a_huge_array_costs_the_range_not_the_container() {
    // THE RUNG'S WHOLE CLAIM. The document holds 20k records; the ceiling could
    // not hold a tenth of them decoded. The run still completes, because the
    // codec cuts the byte region holding exactly the ten in-range elements and
    // re-parses those bytes alone.
    let input = catalog_document(MANY_ELEMENTS);
    assert!(
        input.len() > 512 * 1024,
        "the probe document must be large enough for the ceiling to matter"
    );
    let (run, sink) = run_locate(&input, ".catalog[100:110]", 256 * 1024, 4_096, false);
    let Ok(RangeLocateRun::Completed(report)) = run else {
        panic!("a ten-element range of a large array must complete: {run:?}");
    };
    assert!(matches!(
        report.publication(),
        PublicationStatus::Complete { items: 1, .. }
    ));
    assert_eq!(sink.items, 1);
    let published = String::from_utf8(sink.bytes).expect("published bytes are UTF-8");
    assert!(published.starts_with(r#"[{"id":100,"name":"item-100"},"#));
    assert!(published.ends_with("{\"id\":109,\"name\":\"item-109\"}]\n"));
}

#[test]
fn the_credit_quantum_does_not_change_the_published_bytes() {
    // A one-credit quantum forces a cooperative yield at nearly every step of the
    // validate, the element walk, and the re-parse.
    let input = catalog_document(600);
    let (fine, fine_sink) = run_locate(&input, ".catalog[10:13]", 64 << 20, 1, false);
    let (coarse, coarse_sink) = run_locate(&input, ".catalog[10:13]", 64 << 20, 4_096, false);
    assert!(matches!(fine, Ok(RangeLocateRun::Completed(_))));
    assert!(matches!(coarse, Ok(RangeLocateRun::Completed(_))));
    assert_eq!(
        fine_sink.bytes,
        br#"[{"id":10,"name":"item-10"},{"id":11,"name":"item-11"},{"id":12,"name":"item-12"}]"#
            .iter()
            .copied()
            .chain(*b"\n")
            .collect::<Vec<u8>>()
    );
    assert_eq!(fine_sink.bytes, coarse_sink.bytes);
}

#[test]
fn the_bound_laws_publish_what_the_reference_publishes() {
    for (program, expected) in [
        (
            ".catalog[1:3]",
            r#"[{"id":1,"name":"item-1"},{"id":2,"name":"item-2"}]"#,
        ),
        (".catalog[:1]", r#"[{"id":0,"name":"item-0"}]"#),
        (".catalog[2:]", r#"[{"id":2,"name":"item-2"}]"#),
        // Degenerate: decided without touching one element byte.
        (".catalog[2:2]", "[]"),
        (".catalog[3:1]", "[]"),
        // Wholly past the end, and straddling it.
        (".catalog[9:12]", "[]"),
        (".catalog[2:99]", r#"[{"id":2,"name":"item-2"}]"#),
        // The rounding directions: start floors, end ceils.
        (
            ".catalog[0.7:1.2]",
            r#"[{"id":0,"name":"item-0"},{"id":1,"name":"item-1"}]"#,
        ),
        // Negative bounds resolve against the OBSERVED length in the codec's
        // two-pass arm: count the elements, then wrap. The
        // 3-element document makes the len-relative readings land on exact
        // element boundaries.
        (
            ".catalog[-2:]",
            r#"[{"id":1,"name":"item-1"},{"id":2,"name":"item-2"}]"#,
        ),
        (
            ".catalog[-5:]",
            r#"[{"id":0,"name":"item-0"},{"id":1,"name":"item-1"},{"id":2,"name":"item-2"}]"#,
        ),
        (".catalog[:-2]", r#"[{"id":0,"name":"item-0"}]"#),
        (
            ".catalog[-4:-1]",
            r#"[{"id":0,"name":"item-0"},{"id":1,"name":"item-1"}]"#,
        ),
        // A negative start past the container's start wraps to 0; a negative
        // end at or before the resolved start selects nothing.
        (
            ".catalog[-9:]",
            r#"[{"id":0,"name":"item-0"},{"id":1,"name":"item-1"},{"id":2,"name":"item-2"}]"#,
        ),
        (".catalog[:-9]", "[]"),
    ] {
        let (run, sink) = run_locate(&catalog_document(3), program, 64 << 20, 4_096, false);
        assert!(
            matches!(run, Ok(RangeLocateRun::Completed(_))),
            "{program} must complete: {run:?}"
        );
        assert_eq!(
            String::from_utf8(sink.bytes).expect("utf8"),
            format!("{expected}\n"),
            "{program}"
        );
    }
}

#[test]
fn an_empty_container_publishes_an_empty_array() {
    let (run, sink) = run_locate(br#"{"catalog":[],"meta":1}"#, ".catalog[0:3]", 64 << 20, 4_096, false);
    assert!(matches!(run, Ok(RangeLocateRun::Completed(_))));
    assert_eq!(sink.bytes, b"[]\n");
    // A negative bound over the empty container resolves to the empty range in
    // the two-pass arm's count pass and publishes the same `[]`.
    let (run, sink) = run_locate(br#"{"catalog":[],"meta":1}"#, ".catalog[-3:]", 64 << 20, 4_096, false);
    assert!(matches!(run, Ok(RangeLocateRun::Completed(_))));
    assert_eq!(sink.bytes, b"[]\n");
}

#[test]
fn a_computed_constant_bound_is_byte_identical_to_its_authored_literal() {
    // A computed-but-constant bound folds at lower time, so the
    // computed spelling takes the range-locate rung EXACTLY like the authored
    // literal and publishes the same bytes (the nested `(1+2+2)` folds to 5).
    let input = catalog_document(20);
    let (computed, computed_sink) = run_locate(&input, ".catalog[(1+2):(1+2+2)]", 64 << 20, 4_096, false);
    let (authored, authored_sink) = run_locate(&input, ".catalog[3:5]", 64 << 20, 4_096, false);
    assert!(matches!(computed, Ok(RangeLocateRun::Completed(_))));
    assert!(matches!(authored, Ok(RangeLocateRun::Completed(_))));
    assert_eq!(computed_sink.bytes, authored_sink.bytes);

    // The decline stays: a computed bound that cannot fold is not eligible and
    // is never handed the rung.
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let resources = resources(64 << 20, 4_096);
    let compiled =
        try_compile_program(".catalog[(1*2):]", policy, CompileOptions::new(), &resources).expect("compiles");
    assert!(!compiled.range_locate_eligible());
}

#[test]
fn every_container_dispatch_arm_declines_with_nothing_published() {
    // The dispatch, read as a decline: only a resolved ARRAY is served
    // here. the reference's string slice, its `null` slice, its missing-path `null` and its
    // object slice error are all the ordinary route's, and this rung must reach
    // that route having published nothing at all.
    for (input, expected) in [
        (br#"{"catalog":"abcdef"}"#.as_slice(), "string container"),
        (br#"{"catalog":null}"#.as_slice(), "null container"),
        (br#"{"catalog":{"a":1}}"#.as_slice(), "object container"),
        (br#"{"catalog":7}"#.as_slice(), "number container"),
        (br#"{"catalog":true}"#.as_slice(), "boolean container"),
        (br#"{"other":[1,2,3]}"#.as_slice(), "missing container"),
        // The corrupt-late law: the scoped validate phase is a COMPLETE
        // validator, so a bad byte the range never decodes still stops the run
        // before anything is published.
        (br#"{"catalog":[1,2,3],"junk":01}"#.as_slice(), "corrupt payload"),
    ] {
        let (run, sink) = run_locate(input, ".catalog[0:2]", 64 << 20, 4_096, false);
        assert!(
            matches!(
                run,
                Ok(RangeLocateRun::NotApplicable | RangeLocateRun::NotSingleDocument)
            ),
            "{expected} must decline: {run:?}"
        );
        assert!(sink.bytes.is_empty(), "{expected} published bytes");
        assert_eq!(sink.items, 0, "{expected} opened an item");
    }
}

#[test]
fn adjacent_values_decline_so_the_sequence_path_keeps_owning_them() {
    // The single-document law, in BOTH of its spellings. With adjacency refused
    // by the decode request the whole-input validation fails; with adjacency
    // permitted the codec resolves the first value and the drive still declines,
    // because this rung publishes the whole request in one item and a second
    // value belongs to the sequence path.
    for allow_adjacent_values in [false, true] {
        let (run, sink) = run_locate(
            br#"{"catalog":[1,2]} {"catalog":[3]}"#,
            ".catalog[0:1]",
            64 << 20,
            4_096,
            allow_adjacent_values,
        );
        assert!(
            matches!(
                run,
                Ok(RangeLocateRun::NotApplicable | RangeLocateRun::NotSingleDocument)
            ),
            "adjacency={allow_adjacent_values} must decline: {run:?}"
        );
        assert!(sink.bytes.is_empty());
        assert_eq!(sink.items, 0);
    }
}

#[test]
fn trailing_whitespace_alone_is_still_one_document() {
    // The adjacency check skips exactly the value separators the sequence path
    // skips, so a trailing newline is not a second value.
    let (run, sink) = run_locate(
        b"{\"catalog\":[1,2,3]}\n\n  \t\n",
        ".catalog[0:2]",
        64 << 20,
        4_096,
        true,
    );
    assert!(
        matches!(run, Ok(RangeLocateRun::Completed(_))),
        "trailing whitespace must not decline: {run:?}"
    );
    assert_eq!(sink.bytes, b"[1,2]\n");
}
