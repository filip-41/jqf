//! Compile one program and execute it over a JSON byte slice, collecting
//! the published values. An embedder needs only `jqf-sdk` plus one codec
//! crate — everything below is re-exported from `jqf_sdk`.
//!
//! Run: `cargo run --release -p jqf-sdk --example compile_execute`
//!
//! Errors in this tree are Display-only by design (they render as words,
//! never as boxed std errors), so the example maps them to strings; the
//! one-`expect` setup sites are request invariants, not recoverable paths.

use jqf_codec_json::{self, registration};
use jqf_sdk::{
    CodecCatalog, CompiledProgram, ContinueControl, DecodeRequest, DiagnosticPolicy, DialectId, EncodedItemReport,
    FacadeFraming, FormatId, ItemSink, PipelinePolicy, PreservationRequest, RequestAccount, ResolvedSource,
    ResourceContext, ResourceLimits, SourceId, SourceKind, SourceRef, ValidationMode, WorkMeter, try_compile_program,
};

/// Collects every published item's bytes in order.
struct CollectingSink {
    items: Vec<Vec<u8>>,
}

impl ItemSink for CollectingSink {
    type Error = std::convert::Infallible;
    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        self.items.push(Vec::new());
        Ok(())
    }
    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        self.items
            .last_mut()
            .expect("a write follows begin_item")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn finish_item(&mut self, _index: u64, _report: EncodedItemReport) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn main() -> Result<(), String> {
    fn text<E: std::fmt::Debug>(error: E) -> String {
        format!("{error:?}")
    }

    // The catalog: one codec registration is enough for a JSON pipeline.
    let registration = registration().expect("json registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);

    // The request's resource context (accounting, work meter, limits).
    let control = ContinueControl;
    let mut resources = ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 64 << 20, 0, 128)).map_err(text)?,
        &control,
        WorkMeter::try_new_v1(64).ok_or_else(|| String::from("work meter refused 64 credits"))?,
    )
    .map_err(text)?;

    // Compile: the same entrypoint the CLI uses.
    let policy = jqf_sdk::CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let program: CompiledProgram = try_compile_program(".catalog[].name", policy, &resources).map_err(text)?;

    // The input: a JSON byte slice.
    let input: &[u8] = br#"{"catalog":[{"name":"a"},{"name":"b"}]}"#;
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "example.json",
        input,
        0,
    );
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid");

    // The requirement the program's demand lowers to, and the request policy.
    let requirement = program.try_requirement(&resources).map_err(text)?;
    let pipeline = PipelinePolicy {
        decode: DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: &DialectId::try_new("rfc8259").expect("dialect"),
            options: None,
            // The sequence drive is the adjacent-value route: one or more
            // complete JSON texts.
            allow_adjacent_values: true,
            value_separator: &[],
        },
        encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        encode_options: None,
        cooperative_credits: 64,
        split: None,

        max_iterations: None,
    };

    // Execute: every published value, as it lands.
    let mut sink = CollectingSink { items: Vec::new() };
    let request = jqf_sdk::Request::new(&program, jqf_sdk::Input::Whole(input))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(format(), dialect())
        .with_output_format(format(), dialect())
        .with_policy(pipeline)
        .with_framing(FacadeFraming::item_suffix(b"\n"))
        .with_resources(&mut resources)
        .with_requirement(&requirement);
    let outcome = jqf_sdk::execute(request, &mut sink).map_err(text)?;
    let jqf_sdk::Outcome::Served(jqf_sdk::Report::Sequence(_)) = outcome else {
        return Err("the sequence drive must serve this request".into());
    };

    for item in &sink.items {
        println!("{}", String::from_utf8_lossy(item));
    }
    assert_eq!(sink.items.len(), 2, "one published value per catalog element");
    Ok(())
}
