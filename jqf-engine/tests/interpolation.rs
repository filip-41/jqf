//! String-interpolation parity: the same bytes the current `+` chain emits,
//! which already match system jq.
//!
//! Hole fan-out is RIGHT-outer (`+`'s left operand is the fastest-varying
//! loop). These rows pin that order, empty holes, tostring of non-strings,
//! and a hole raise that names the hole rather than a synthetic `+`.

use jqf_codec_core::{DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
use jqf_data::{DialectId, FormatId};
use jqf_engine::{CodecRequirementPolicy, try_compile_program};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_sdk::{CodecCatalog, Diagnostics, EncodedItemReport, FacadeFraming, ItemSink, PipelinePolicy};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

const COOPERATIVE_CREDITS: u32 = 64;
static CONTROL: ContinueControl = ContinueControl;

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
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(COOPERATIVE_CREDITS).expect("work"),
    )
    .expect("resources")
}

fn run_to_json(program: &str, input: &str) -> String {
    let mut resources = resources();
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
        decode: DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: &DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect"),
            options: None,
            allow_adjacent_values: false,
            value_separator: jqf_codec_json::VALUE_SEPARATORS,
        },
        encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::Report,
        encode_options: None,
        cooperative_credits: COOPERATIVE_CREDITS,
        split: None,

        max_iterations: None,
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
        Ok(_) => String::from_utf8(sink.bytes).expect("utf-8 output"),
        Err(error) => panic!(
            "expected success for {program:?} over {input:?}, got: {:?}",
            error.pipeline_failure()
        ),
    }
}

fn run_expect_error(program: &str, input: &str) -> String {
    let mut resources = resources();
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
        decode: DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: &DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect"),
            options: None,
            allow_adjacent_values: false,
            value_separator: jqf_codec_json::VALUE_SEPARATORS,
        },
        encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::Report,
        encode_options: None,
        cooperative_credits: COOPERATIVE_CREDITS,
        split: None,

        max_iterations: None,
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
        Ok(_) => panic!(
            "expected an error for {program:?} over {input:?}, got output: {}",
            String::from_utf8(sink.bytes).expect("utf-8 output")
        ),
        Err(error) => format!("{:?}", error.pipeline_failure()),
    }
}

#[test]
fn a_static_template_is_the_literal() {
    assert_eq!(run_to_json(r#""a""#, "null"), "\"a\"\n");
    assert_eq!(run_to_json(r#""""#, "null"), "\"\"\n");
}

#[test]
fn leftmost_hole_varies_fastest() {
    // `"\(1,2)\(3,4)\(5,6)"` is `"135" "235" "145" "245" "136" …`.
    assert_eq!(
        run_to_json(r#"["\(1,2)\(3,4)"]"#, "null"),
        "[\"13\",\"23\",\"14\",\"24\"]\n"
    );
    assert_eq!(
        run_to_json(r#"["\(1,2)\(3,4)\(5,6)"]"#, "null"),
        "[\"135\",\"235\",\"145\",\"245\",\"136\",\"236\",\"146\",\"246\"]\n"
    );
    assert_eq!(
        run_to_json(r#"["\(1,2)\(3,4)"] == [(("" + ("1","2")) + ("3","4"))]"#, "null"),
        "true\n"
    );
}

#[test]
fn empty_holes_and_empty_literal_parts() {
    assert_eq!(run_to_json(r#""\("")""#, "null"), "\"\"\n");
    assert_eq!(run_to_json(r#""a\("")b""#, "null"), "\"ab\"\n");
    assert_eq!(run_to_json(r#""\(1)\("")\(2)""#, "null"), "\"12\"\n");
    assert_eq!(run_to_json(r#"["\(empty)x"]"#, "null"), "[]\n");
    assert_eq!(run_to_json(r#"["x\(empty)y\(1)"]"#, "null"), "[]\n");
}

#[test]
fn holes_stringify_non_strings() {
    assert_eq!(run_to_json(r#""\(null)\(true)""#, "null"), "\"nulltrue\"\n");
    assert_eq!(run_to_json(r#""\([1,2])""#, "null"), "\"[1,2]\"\n");
    assert_eq!(run_to_json(r#""\(1)\("x")""#, "null"), "\"1x\"\n");
}

#[test]
fn a_hole_raise_names_the_hole_not_a_synthetic_add() {
    let caught = run_to_json(r#"try "pre\(error("hole"))post" catch ."#, "null");
    assert_eq!(caught, "\"hole\"\n");
    let raised = run_expect_error(r#""pre\(error("hole"))post""#, "null");
    assert!(raised.contains("hole"), "the hole's payload must appear: {raised}");
    assert!(
        !raised.to_ascii_lowercase().contains("cannot be added") && !raised.to_ascii_lowercase().contains("cannot add"),
        "a hole raise must not be attributed to a synthetic +: {raised}"
    );
}

#[test]
fn accessor_holes_compile() {
    try_compile_program(
        r#""\(.a.@tag)""#,
        CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly),
        &resources(),
    )
    .expect(r#""\(.a.@tag)" must compile"#);
    try_compile_program(
        r#""\(.a.&href)""#,
        CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly),
        &resources(),
    )
    .expect(r#""\(.a.&href)" must compile"#);
}
