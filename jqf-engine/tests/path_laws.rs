//! The five path-provenance laws, pinned end to end through the SDK.
//!
//! The laws and their engine-side prose live in
//! `jqf-engine/src/exec/path_register.rs`: Law 1 (provenance is dynamic,
//! decided by identity, never by value equality), Law 2 (navigation over an
//! untracked value raises), Law 3 (emission of an untracked value raises),
//! Law 4 (`foreach` extract emits the item's path), Law 5 (the `reduce`
//! update runs with the register on the source item). One test here carries
//! each number in its name; the frozen-subexpression tests pin the related
//! rule that subexpression positions evaluate with the register frozen.
//!
//! The helpers live in this file rather than in the crate: `test_support` does
//! not exist in `jqf_engine`, and this pin must not grow the crate's public
//! surface. Every case drives `jqf_sdk::execute` exactly like `jqf-sdk/tests/`
//! does.

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

/// Runs `program` over `input` through the SDK's `execute` (JSON in, JSON
/// out), returning the published output text on success.
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
            value_separator: &[],
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

/// Runs `program` over `input` through the SDK's `execute`, returning the
/// failure's rendered message; panics if the run succeeds.
///
/// The path-law rejections travel as RAISED string values, so the message is
/// the raised value itself; every other failure class renders through
/// [`jqf_sdk::PipelineFailure`]'s prose Display. Matching this text — what a
/// host would actually show — keeps the assertions off `Debug` spellings.
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
            value_separator: &[],
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
        Err(error) => match error.pipeline_failure() {
            Some(jqf_sdk::PipelineFailure::Raised(raised)) => match raised.value() {
                jqf_data::Value::String(text) => text.to_string(),
                other => format!("{other:?}"),
            },
            Some(failure) => failure.to_string(),
            None => format!("{:?}", error.pipeline_failure()),
        },
    }
}

#[test]
fn law_1_provenance_tracks_the_tracked_value_not_an_equal_one() {
    // Provenance is dynamic, decided by identity at every navigation and
    // emission: the bound value still IS the value at the empty path, so its
    // emission publishes the register (`[]`), while a literal that merely
    // EQUALS the current value is a fresh value and raises instead.
    assert_eq!(run_to_json(r"path(. as $x | $x)", r"2").trim_end(), r"[]");
    let error = run_expect_error(r"path(2)", r"2");
    assert!(error.contains("Invalid path expression with result"), "got: {error}");
}

#[test]
fn law_2_navigation_over_an_untracked_value_raises() {
    // The navigation must sit INSIDE `path()` for path mode to see it:
    // `path(.)` alone is the identity path, so the program answers `[]` with
    // exit 0. With that spelling this implementation raises the error below.
    let error = run_expect_error(r"path(.a as $x | $x.b)", r#"{"a":{"b":1}}"#);
    assert!(
        error.contains("Invalid path expression near attempt to access element"),
        "got: {error}"
    );
}

#[test]
fn law_3_emission_of_an_untracked_value_raises() {
    let error = run_expect_error(r"path(1)", r"null");
    assert!(error.contains("Invalid path expression with result"), "got: {error}");
}

#[test]
fn law_4_foreach_extract_emits_the_items_path() {
    assert_eq!(
        run_to_json(r"[path(foreach .a[] as $i (0; .+1; $i))]", r#"{"a":[10,20]}"#).trim_end(),
        r#"[["a",0],["a",1]]"#
    );
}

#[test]
fn law_5_reduce_update_sits_on_the_source_item() {
    let error = run_expect_error(r"path(reduce .a.b as $x (.; .a))", r#"{"a":{"b":1}}"#);
    assert!(error.contains("Invalid path expression"), "got: {error}");
}

#[test]
fn frozen_subexpression_binder_source_is_a_value_not_a_location() {
    // `. as $x | $x.a` is legal: $x is still, by identity, the value at the
    // empty path.
    assert_eq!(
        run_to_json(r"[path(. as $x | $x.a)]", r#"{"a":1}"#).trim_end(),
        r#"[["a"]]"#
    );
}

#[test]
fn frozen_subexpression_conditional_condition_is_frozen() {
    assert_eq!(
        run_to_json(r"[path(if .a then .b else .c end)]", r#"{"a":true,"b":1,"c":2}"#).trim_end(),
        r#"[["b"]]"#
    );
}

#[test]
fn frozen_subexpression_builtin_argument_is_frozen() {
    assert_eq!(
        run_to_json(r"[path(.[] | select(.x))]", r#"[{"x":true}]"#).trim_end(),
        r"[[0]]"
    );
}
