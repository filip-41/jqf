//! `$__loc__` serves OPERAND positions through the same expression ladder as
//! everywhere else.
//!
//! `$__loc__` is accepted wherever a term is allowed. An earlier compile
//! accepted it only in expression position and refused `.[$__loc__]` by name
//! at the operand ladder while `.[$ENV]` lowered there — an internal
//! inconsistency this file pins shut. The aligned law: `.[$__loc__]`, `.@($__loc__)`, and
//! their slice twins lower through [`bind_operand`]'s frame exactly like any
//! general expression, carrying the reference site's own `{file,line}`
//! literal, and the runtime decides key-versus-mismatch exactly as for every
//! other bound value.
//!
//! The helpers live in this file rather than in the crate: `test_support`
//! does not exist in `jqf_engine`, and this pin must not grow the crate's
//! public surface. Every case drives `jqf_sdk::execute` exactly like
//! `jqf-engine/tests/path_laws.rs` does.

use jqf_codec_core::{DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
use jqf_data::{DialectId, FormatId};
use jqf_engine::{CodecRequirementPolicy, CompileOptions, EngineCompileError, try_compile_program};
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

/// The shared request build behind both drives below: JSON in, JSON out,
/// whole-input, strict validation.
fn run(program: &str, input: &str) -> Result<String, String> {
    let mut resources = resources();
    let registration = jqf_codec_json::registration().expect("json registration");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id");
    let compiled = try_compile_program(
        program,
        CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly),
        CompileOptions::new(),
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
        Ok(_) => Ok(String::from_utf8(sink.bytes).expect("utf-8 output")),
        Err(error) => Err(match error.pipeline_failure() {
            Some(failure) => failure.to_string(),
            None => format!("{:?}", error.pipeline_failure()),
        }),
    }
}

#[test]
fn loc_index_compiles_and_answers_null_over_null() {
    // `null | .[$__loc__].line` answers `null` — indexing null is null, and
    // `.line` over null is null. The spelling used to refuse at compile.
    assert_eq!(run(r".[$__loc__].line", r"null").expect("runs"), "null\n");
}

#[test]
fn per_element_loc_access_streams_each_element() {
    // `[null,null] | [.[] | .[$__loc__].line]` answers `[null,null]` — each
    // element takes the operand independently.
    assert_eq!(
        run(r"[.[] | .[$__loc__].line]", r"[null,null]").expect("runs"),
        "[null,null]\n"
    );
}

#[test]
fn operand_twin_answers_what_expression_position_answers() {
    // Over null every bound type indexes to null, so the operand spelling
    // must answer exactly its expression-position twin: the literal itself,
    // and null through the frame.
    assert_eq!(
        run(r"$__loc__", r"null").expect("runs").trim_end(),
        r#"{"file":"<top-level>","line":1}"#
    );
    assert_eq!(run(r".[$__loc__].line", r"null").expect("runs"), "null\n");
}

#[test]
fn object_container_still_raises_the_index_class_mismatch() {
    // Acceptance is compile-time only: indexing an object with the location
    // object is the ordinary index-class mismatch at run time
    // (`Cannot index object with object …`), surfaced here through the
    // SDK's typed TypeMismatch display.
    let error = run(r".[$__loc__]", r#"{"a":1}"#).expect_err("raises");
    assert!(error.to_lowercase().contains("cannot index"), "got: {error}");
}

#[test]
fn the_old_refusal_is_gone_and_unknown_names_still_refuse() {
    let resources = resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    for program in [".[$__loc__]", ".@($__loc__)", ".[$__loc__:]", "$__loc__"] {
        assert!(
            try_compile_program(program, policy, CompileOptions::new(), &resources).is_ok(),
            "{program} must compile"
        );
    }
    // A genuinely unbound name keeps its identical refusal from the same
    // ladder the named bindings now flow through.
    match try_compile_program(".[$nope]", policy, CompileOptions::new(), &resources) {
        Err(EngineCompileError::UndefinedVariable { .. }) => {}
        other => panic!("expected UndefinedVariable, got {other:?}"),
    }
}
