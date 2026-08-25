//! The source-preserving round-trip lane — SDK coverage.
//!
//! `execute_source_roundtrip` publishes the retained input bytes VERBATIM when
//! the program is provably the identity filter and the whole input is exactly
//! one document of the same format/dialect pair the output asks for. Every
//! other shape declines (`RoundtripRun::Declined`) with nothing published, and
//! a malformed single document fails exactly as the floor does. These tests
//! drive the public SDK surface with the real JSON codec, the same way the CLI
//! round-trip lane drives it.

/// A process-lifetime built-in dialect for request construction (123 X5).
fn json_dialect() -> &'static DialectId {
    Box::leak(Box::new(DialectId::try_new("rfc8259").expect("dialect")))
}

use jqf_codec_core::{DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
use jqf_codec_json::{JsonEncodeOptions, JsonIndent};
use jqf_data::{DialectId, FormatId};
use jqf_engine::{CodecRequirementPolicy, try_compile_program};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_sdk::{
    CodecCatalog, EncodedItemReport, FacadeFraming, Input, ItemSink, Outcome, PipelineFailure, PipelinePolicy, Report,
    Request,
};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

/// Local mirror of the drive-level round-trip outcome: the drive type is
/// crate-private, so tests reconstruct it from `Outcome` exactly as
/// request.rs does.
#[derive(Debug)]
// The mirror exists so `matches!` arms keep their names; not every variant
// is constructed or read by every suite.
#[allow(dead_code)]
enum RoundtripRun {
    Published(jqf_sdk::PipelineReport),
    Encoded(jqf_sdk::PipelineReport),
    Declined,
}

const COOPERATIVE_CREDITS: u32 = 64;
static CONTROL: ContinueControl = ContinueControl;

/// Collects every published byte and counts item boundaries.
struct CollectingSink {
    bytes: Vec<u8>,
    items: usize,
    last_report: Option<EncodedItemReport>,
}

impl CollectingSink {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            items: 0,
            last_report: None,
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

    fn finish_item(&mut self, _index: u64, report: EncodedItemReport) -> Result<(), Self::Error> {
        self.last_report = Some(report);
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
    PipelinePolicy {
        decode: DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: json_dialect(),
            options: None,
            allow_adjacent_values: true,
            value_separator: jqf_codec_json::VALUE_SEPARATORS,
        },
        encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        encode_options: None,
        cooperative_credits: COOPERATIVE_CREDITS,
        split: None,

        max_iterations: None,
    }
}

/// Runs the round-trip entry over `input` with the strict-JSON RFC 8259 pair on
/// both sides, writing any published bytes into `sink`.
fn run_roundtrip_with(
    input: &[u8],
    sink: &mut CollectingSink,
    program_source: &str,
) -> Result<RoundtripRun, jqf_sdk::PipelineError<String>> {
    run_roundtrip_with_policy(input, sink, program_source, policy())
}

fn run_roundtrip_with_policy(
    input: &[u8],
    sink: &mut CollectingSink,
    program_source: &str,
    pipeline_policy: PipelinePolicy<'static>,
) -> Result<RoundtripRun, jqf_sdk::PipelineError<String>> {
    let registration = jqf_codec_json::registration().expect("json registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid");
    let mut resources = resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled = try_compile_program(program_source, policy, &resources).expect("program compiles");
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.json",
        input,
        0,
    );
    let request = Request::new(&compiled, Input::Whole(input))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(format(), dialect())
        .with_output_format(format(), dialect())
        .with_policy(pipeline_policy)
        .with_framing(FacadeFraming::item_suffix(b"\n"))
        .with_resources(&mut resources)
        .roundtrip();
    match jqf_sdk::execute(request, sink) {
        Ok(Outcome::Served(Report::Pipeline(report))) => Ok(RoundtripRun::Published(report)),
        Ok(Outcome::Served(other)) => panic!("unexpected drive report: {other:?}"),
        Ok(Outcome::Declined) => Ok(RoundtripRun::Declined),
        Err(error) => match error {
            jqf_sdk::Failure::Pipeline(error) => Err(error),
            other => panic!("unexpected failure class: {other:?}"),
        },
    }
}

/// Runs the round-trip entry over `input` with the strict-JSON RFC 8259 pair on
/// both sides, writing any published bytes into `sink`, and returns the
/// published bytes (or panics if the lane declined).
fn echoed(input: &[u8]) -> Vec<u8> {
    let mut sink = CollectingSink::new();
    let run = run_roundtrip_with(input, &mut sink, ".").expect("identity over one document round-trips");
    let RoundtripRun::Published(_) = run else {
        panic!("a canonical identity must publish, got {run:?}");
    };
    sink.bytes
}

/// Runs the round-trip entry over `input` and asserts it DECLINES with nothing
/// published.
fn declined(input: &[u8]) {
    let mut sink = CollectingSink::new();
    let run = run_roundtrip_with(input, &mut sink, ".").expect("identity over one document round-trips");
    assert!(
        matches!(run, RoundtripRun::Declined),
        "a non-canonical source must decline, got {run:?}"
    );
    assert!(sink.bytes.is_empty(), "nothing may be published on decline");
    assert_eq!(sink.items, 0);
}

#[test]
fn decimal_spellings_that_are_their_own_render_echo() {
    // The canonical-identity matrix: every spelling whose compact render is
    // itself must echo. The render plan's coefficient range straddles the
    // fraction point for every nonzero integer part (`1.5` names the range
    // `1.5`), so this is the family the straddle bug declined — the echo fired
    // only on `0.5`-shaped inputs whose first non-zero digit sits past the
    // point. An all-zero magnitude (`0.0`, `-0.0`) names the EMPTY range whose
    // coefficient is the sign-carrying zero, the second family the old code
    // declined (the empty slice rendered no digits at all).
    for input in [
        b"[0.5]".as_slice(),
        b"[1.5]",
        b"[10.5]",
        b"[1.05]",
        b"[-1.5]",
        b"[-0.5]",
        b"[0.0]",
        b"[-0.0]",
        b"[100.0]",
        b"[0.100]",
        b"{\"a\":1.5}",
    ] {
        let mut echoed = echoed(input);
        echoed.pop(); // the facade suffix
        assert_eq!(echoed, input, "the echoed value bytes must be the source for {input:?}");
    }
}

#[test]
fn a_trailing_newline_does_not_lose_the_echo() {
    // The trailing line feed is the facade's suffix, never part of the echoed
    // value bytes: a canonical document with a trailing newline still echoes.
    assert_eq!(echoed(b"[1.5]\n"), b"[1.5]\n");
}

#[test]
fn non_self_render_spellings_decline() {
    // The disqualifier side of the matrix: exponent numbers (`1e3` renders
    // `1000`), non-minimal escapes, duplicate keys, and nine-key objects all
    // clear the canonicality flag and must decline to the caller's floor.
    for input in [
        b"[1e3]".as_slice(),
        b"[1.5e3]",
        b"[\"a\\/b\"]",
        b"[\"\\u0041\"]",
        b"{\"a\":1,\"a\":2}",
        b"{\"a0\":0,\"a1\":1,\"a2\":2,\"a3\":3,\"a4\":4,\"a5\":5,\"a6\":6,\"a7\":7,\"a8\":8}",
        // A non-finite spelling renders `null`, never itself. Echoing it
        // republished `NaN`/`Infinity` — bytes no JSON reader accepts.
        b"{\"a\":NaN}",
        b"[Infinity]",
        b"[-inf]",
        b"[snan]",
        b"nan",
        // A raw DEL is admitted unescaped by the grammar but sits in the
        // encoder's escape set, so it is never its own render either — and
        // `keys` over the same document already spelled it escaped.
        b"[\"a\x7fb\"]",
        b"{\"k\x7f\":1}",
    ] {
        declined(input);
    }
}

#[test]
fn non_canonical_identity_declines() {
    // The S4 canonicality gate: a source whose compact render is not itself
    // (interior whitespace here) must NOT be echoed — the caller's floor
    // renders authoritative bytes. The lane publishes nothing and declines.
    let input = b"{\n  \"a\" : [1, 2, 3]\n}";
    let mut sink = CollectingSink::new();
    let run = run_roundtrip_with(input, &mut sink, ".").expect("identity over one document round-trips");
    assert!(
        matches!(run, RoundtripRun::Declined),
        "a non-canonical source must decline, got {run:?}"
    );
    assert!(sink.bytes.is_empty(), "nothing may be published on decline");
    assert_eq!(sink.items, 0);
}

#[test]
fn identity_echoes_value_bytes_with_facade_framing() {
    // The canonical identity is the source's VALUE bytes (the source up to
    // the consumed offset) followed by the facade's own item suffix — the
    // same shape the reference's render-plus-newline takes.
    let input = b"{\"a\":1}";
    let mut sink = CollectingSink::new();
    let run = run_roundtrip_with(input, &mut sink, ".").expect("identity over one document round-trips");
    let RoundtripRun::Published(report) = run else {
        panic!("a canonical identity must publish, got {run:?}");
    };
    assert_eq!(sink.bytes, b"{\"a\":1}\n", "value bytes plus the facade suffix");
    assert_eq!(sink.items, 1);
    assert_eq!(
        report.publication(),
        jqf_sdk::PublicationStatus::Complete {
            items: 1,
            published_bytes: u64::try_from(sink.bytes.len()).expect("fits a u64"),
        }
    );
}

#[test]
fn identity_excludes_trailing_whitespace() {
    // The trailing whitespace is the facade's newline's job, exactly as the
    // render's trailing newline is the reference's; a trailing line feed must not make a
    // canonical file-shaped input decline (that would forfeit the echo on
    // exactly the inputs that have it).
    let input = b"42  \n";
    let mut sink = CollectingSink::new();
    run_roundtrip_with(input, &mut sink, ".").expect("identity over one document round-trips");
    assert_eq!(sink.bytes, b"42\n", "value bytes plus the facade suffix");
}

#[test]
fn identity_excludes_leading_whitespace() {
    // The other end of the same law: the separator scan skips whatever leads
    // up to the value, so the echoed bytes start AT the value. Publishing from
    // byte zero republished indentation the compact render never emits — one
    // document, two spellings, decided by which lane served it.
    for (input, echoed) in [
        (b" {\"a\":1}".as_slice(), b"{\"a\":1}\n".as_slice()),
        (b"\n\t 42\n", b"42\n"),
        (b"\r\n[1,2]", b"[1,2]\n"),
        (b"  \"s\"  \n", b"\"s\"\n"),
    ] {
        let mut sink = CollectingSink::new();
        run_roundtrip_with(input, &mut sink, ".").expect("identity round-trips");
        assert_eq!(sink.bytes, echoed, "leading whitespace echoed for {input:?}");
    }
}

#[test]
fn non_identity_program_declines_without_publishing() {
    let mut sink = CollectingSink::new();
    let run = run_roundtrip_with(b"{\"a\":1}", &mut sink, ".a").expect("a declined lane is not an error");
    assert!(matches!(run, RoundtripRun::Declined));
    assert!(sink.bytes.is_empty(), "a decline must publish nothing");
    assert_eq!(sink.items, 0);
}

#[test]
fn adjacent_values_decline_without_publishing() {
    for input in [b"1 2".as_slice(), b"1\n2\n".as_slice(), b"{} {}".as_slice()] {
        let mut sink = CollectingSink::new();
        let run = run_roundtrip_with(input, &mut sink, ".").expect("a declined lane is not an error");
        assert!(matches!(run, RoundtripRun::Declined), "input {input:?}");
        assert!(sink.bytes.is_empty(), "a decline must publish nothing");
    }
}

#[test]
fn a_malformed_second_value_declines_without_publishing() {
    // The first value is complete, so the input is a multi-text stream; the
    // lane declines and the caller's floor owns the malformed tail's failure.
    let mut sink = CollectingSink::new();
    let run = run_roundtrip_with(b"1 {", &mut sink, ".").expect("a declined lane is not an error");
    assert!(matches!(run, RoundtripRun::Declined));
    assert!(sink.bytes.is_empty());
}

#[test]
fn empty_input_declines_without_publishing() {
    let mut sink = CollectingSink::new();
    let run = run_roundtrip_with(b"", &mut sink, ".").expect("a declined lane is not an error");
    assert!(matches!(run, RoundtripRun::Declined));
    assert!(sink.bytes.is_empty());
}

#[test]
fn malformed_single_document_fails_like_the_floor() {
    let mut sink = CollectingSink::new();
    let error = run_roundtrip_with(b"{\"a\":}", &mut sink, ".")
        .expect_err("a malformed single document must fail, not decline");
    assert!(
        matches!(error.failure(), PipelineFailure::Codec(_)),
        "the failure must be the codec's, got {error:?}"
    );
    assert!(sink.bytes.is_empty(), "nothing may publish before validation");
}

#[test]
fn different_output_format_or_dialect_declines() {
    let registration = jqf_codec_json::registration().expect("json registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid");
    let other_format = FormatId::try_new("json2").expect("synthetic format id is valid");
    let other_dialect = DialectId::try_new("dialect2").expect("synthetic dialect id is valid");
    let mut resources = resources();
    let requirement_policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled = try_compile_program(".", requirement_policy, &resources).expect("program compiles");
    let mut sink = CollectingSink::new();
    let run = run_roundtrip_declines(
        &catalog,
        &compiled,
        format(),
        dialect(),
        other_format,
        dialect(),
        &mut resources,
        &mut sink,
    );
    assert!(matches!(run, RoundtripRun::Declined));
    assert!(sink.bytes.is_empty());

    let mut sink = CollectingSink::new();
    let run = run_roundtrip_declines(
        &catalog,
        &compiled,
        format(),
        dialect(),
        format(),
        other_dialect,
        &mut resources,
        &mut sink,
    );
    assert!(matches!(run, RoundtripRun::Declined));
    assert!(sink.bytes.is_empty());
}

/// Runs the round-trip lane with an explicit output format/dialect pair,
/// returning the lane's verdict (the pair mismatch must decline).
#[expect(
    clippy::too_many_arguments,
    reason = "the decline probe forwards the route context's format/dialect/policy inventory"
)]
fn run_roundtrip_declines(
    catalog: &CodecCatalog<'_, '_>,
    compiled: &jqf_engine::CompiledProgram,
    input_format: FormatId,
    input_dialect: DialectId,
    output_format: FormatId,
    output_dialect: DialectId,
    resources: &mut ResourceContext<'static>,
    sink: &mut CollectingSink,
) -> RoundtripRun {
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.json",
        b"42",
        0,
    );
    let request = Request::new(compiled, Input::Whole(b"42"))
        .with_catalog(*catalog)
        .with_source(source)
        .with_format(input_format, input_dialect)
        .with_output_format(output_format, output_dialect)
        .with_policy(policy())
        .with_framing(FacadeFraming::item_suffix(b"\n"))
        .with_resources(resources)
        .roundtrip();
    match jqf_sdk::execute(request, sink).expect("a declined lane is not an error") {
        Outcome::Served(Report::Pipeline(report)) => RoundtripRun::Published(report),
        Outcome::Served(other) => panic!("unexpected drive report: {other:?}"),
        Outcome::Declined => RoundtripRun::Declined,
    }
}

#[test]
fn default_execute_echoes_canonical_identity_without_the_roundtrip_flag() {
    // FFI/Python call execute once with no exclusive rung flag; the default
    // ladder still takes the echo.
    let registration = jqf_codec_json::registration().expect("json registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid");
    let mut resources = resources();
    let compiled = try_compile_program(
        ".",
        CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .expect("program compiles");
    let requirement = compiled.try_requirement(&resources).expect("identity lowers");
    let input = b"{\"a\":1}";
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.json",
        input,
        0,
    );
    let mut sink = CollectingSink::new();
    let request = Request::new(&compiled, Input::Whole(input))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(format(), dialect())
        .with_output_format(format(), dialect())
        .with_policy(policy())
        .with_framing(FacadeFraming::item_suffix(b"\n"))
        .with_resources(&mut resources)
        .with_requirement(&requirement);
    let outcome = jqf_sdk::execute(request, &mut sink).expect("default ladder serves");
    assert!(matches!(outcome, Outcome::Served(Report::Pipeline(_))));
    assert_eq!(sink.bytes, b"{\"a\":1}\n");
}

#[test]
fn sort_keys_declines_the_echo() {
    // `-S` reorders members; the retained source is no longer the answer.
    // The encoder factory answers the canonical-form question, so the
    // request carries no JSON vocabulary bools.
    static SORT: JsonEncodeOptions = JsonEncodeOptions {
        indent: JsonIndent::Compact,
        raw_strings: false,
        sort_keys: true,
        ascii_output: false,
        raw_output_nul: false,
    };
    let mut pipeline_policy = policy();
    pipeline_policy.encode_options = Some(&SORT);
    let mut sink = CollectingSink::new();
    let run = run_roundtrip_with_policy(b"{\"b\":1,\"a\":2}", &mut sink, ".", pipeline_policy)
        .expect("a declined lane is not an error");
    assert!(matches!(run, RoundtripRun::Declined));
    assert!(sink.bytes.is_empty());
}

#[test]
fn pretty_indent_declines_the_echo() {
    static PRETTY: JsonEncodeOptions = JsonEncodeOptions {
        indent: JsonIndent::Spaces(2),
        raw_strings: false,
        sort_keys: false,
        ascii_output: false,
        raw_output_nul: false,
    };
    let mut pipeline_policy = policy();
    pipeline_policy.encode_options = Some(&PRETTY);
    let mut sink = CollectingSink::new();
    let run = run_roundtrip_with_policy(b"{\"a\":1}", &mut sink, ".", pipeline_policy)
        .expect("a declined lane is not an error");
    assert!(matches!(run, RoundtripRun::Declined));
    assert!(sink.bytes.is_empty());
}

#[test]
fn adjacent_value_opt_in_off_declines() {
    let mut policy = policy();
    policy.decode.allow_adjacent_values = false;
    let mut sink = CollectingSink::new();
    let run = run_roundtrip_with_policy(b"42", &mut sink, ".", policy).expect("a declined lane is not an error");
    assert!(matches!(run, RoundtripRun::Declined));
    assert!(sink.bytes.is_empty());
}

#[test]
fn echo_judges_located_truthiness() {
    // The echo holds a Located value; `-e` reads value_truthy from that
    // report. Fabricating None would make a falsy file exit 0 while stdin
    // and the sequence floor both exit 1.
    for (input, truthy, empty_array) in [
        (b"null".as_slice(), false, false),
        (b"false", false, false),
        (b"true", true, false),
        (b"0", true, false),
        (b"[]", true, true),
        (b"{}", true, false),
        (b"\"\"", true, false),
    ] {
        let mut sink = CollectingSink::new();
        let run = run_roundtrip_with(input, &mut sink, ".").expect("canonical identity echoes");
        assert!(
            matches!(run, RoundtripRun::Published(_)),
            "expected publish for {}",
            input.escape_ascii()
        );
        let report = sink.last_report.expect("finish_item ran");
        assert_eq!(report.value_truthy(), Some(truthy), "{}", input.escape_ascii());
        assert_eq!(
            report.value_empty_array(),
            Some(empty_array),
            "{}",
            input.escape_ascii()
        );
    }
}
