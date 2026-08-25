//! The JSON encoder's indentation contract, driven through the public SDK
//! surface the CLI uses. `tools/jqf-cli-jq-compat.sh` proves these bytes equal
//! the reference's on a real corpus, but that gate needs a `jq` on PATH; this file pins the
//! same contract deterministically in-tree so a break fails `cargo test`.
//!
//! The codec's own default is compact — the two-space default a `jqf` user sees
//! is the CLI's choice, passed down as encode options.

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
    CodecCatalog, EncodedItemReport, FacadeFraming, Input, ItemSink, Outcome, PipelinePolicy, Report, Request,
};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

const COOPERATIVE_CREDITS: u32 = 64;
static CONTROL: ContinueControl = ContinueControl;

struct CollectingSink {
    bytes: Vec<u8>,
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

/// Runs `.` over `input` and returns every published byte, encoding with
/// `indent` when one is named and with the codec's own default otherwise.
fn render(input: &[u8], indent: Option<JsonIndent>) -> String {
    let registration = jqf_codec_json::registration().expect("json registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid");
    let mut resources = ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 64 << 20, 0, 512)).expect("account allocates"),
        &CONTROL,
        WorkMeter::try_new_v1(COOPERATIVE_CREDITS).expect("work meter starts"),
    )
    .expect("resources start");
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled = try_compile_program(".", policy, &resources).expect("program compiles");
    let requirement = compiled.try_requirement(&resources).expect("requirement lowers");
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.json",
        input,
        0,
    );
    let options = indent.map(|indent| JsonEncodeOptions {
        indent,
        raw_strings: false,
        sort_keys: false,
        ascii_output: false,
        raw_output_nul: false,
    });
    let mut sink = CollectingSink { bytes: Vec::new() };
    let request = Request::new(&compiled, Input::Whole(input))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(format(), dialect())
        .with_output_format(format(), dialect())
        .with_policy(PipelinePolicy {
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
            encode_options: options
                .as_ref()
                .map(|value| value as &(dyn core::any::Any + Send + Sync)),
            cooperative_credits: COOPERATIVE_CREDITS,
            split: None,

            max_iterations: None,
        })
        .with_framing(FacadeFraming::item_suffix(b"\n"))
        .with_resources(&mut resources)
        .with_requirement(&requirement);
    match jqf_sdk::execute(request, &mut sink) {
        Ok(Outcome::Served(Report::Sequence(_) | Report::Pipeline(_))) => {}
        Ok(Outcome::Served(other)) => panic!("unexpected drive report: {other:?}"),
        Ok(Outcome::Declined) => panic!("the sequence drive must not decline"),
        Err(error) => panic!("the sequence must publish: {error}"),
    }
    String::from_utf8(sink.bytes).expect("published bytes are UTF-8")
}

const NESTED: &[u8] = br#"{"a":1,"b":[2,{"c":"x"}],"d":{},"e":[]}"#;

#[test]
fn omitted_options_select_the_codecs_own_compact_default() {
    assert_eq!(
        render(NESTED, None),
        concat!(r#"{"a":1,"b":[2,{"c":"x"}],"d":{},"e":[]}"#, "\n")
    );
}

#[test]
fn compact_named_explicitly_matches_omitting_the_options() {
    assert_eq!(render(NESTED, Some(JsonIndent::Compact)), render(NESTED, None));
}

#[test]
fn two_spaces_is_jqs_default_rendering() {
    assert_eq!(
        render(NESTED, Some(JsonIndent::Spaces(2))),
        "{\n  \"a\": 1,\n  \"b\": [\n    2,\n    {\n      \"c\": \"x\"\n    }\n  ],\n  \
         \"d\": {},\n  \"e\": []\n}\n"
    );
}

#[test]
fn zero_spaces_still_breaks_lines_and_is_not_compact() {
    // the reference's `--indent 0`: every break a pretty run would take, with nothing to
    // indent it by. The `": "` after a key stays, which is what separates this
    // from compact output.
    assert_eq!(
        render(NESTED, Some(JsonIndent::Spaces(0))),
        "{\n\"a\": 1,\n\"b\": [\n2,\n{\n\"c\": \"x\"\n}\n],\n\"d\": {},\n\"e\": []\n}\n"
    );
}

#[test]
fn tabs_indent_one_tab_per_level() {
    assert_eq!(
        render(NESTED, Some(JsonIndent::Tabs)),
        "{\n\t\"a\": 1,\n\t\"b\": [\n\t\t2,\n\t\t{\n\t\t\t\"c\": \"x\"\n\t\t}\n\t],\n\t\
         \"d\": {},\n\t\"e\": []\n}\n"
    );
}

#[test]
fn scalars_and_empty_containers_at_the_root_never_break() {
    for indent in [JsonIndent::Spaces(2), JsonIndent::Tabs, JsonIndent::Spaces(7)] {
        assert_eq!(render(b"1", Some(indent)), "1\n");
        assert_eq!(render(br#""x""#, Some(indent)), "\"x\"\n");
        assert_eq!(render(b"null", Some(indent)), "null\n");
        assert_eq!(render(b"[]", Some(indent)), "[]\n");
        assert_eq!(render(b"{}", Some(indent)), "{}\n");
    }
}

/// `depth` nested arrays around a single `1`.
fn nested_arrays(depth: usize) -> Vec<u8> {
    let mut bytes = vec![b'['; depth];
    bytes.push(b'1');
    bytes.resize(bytes.len() + depth, b']');
    bytes
}

/// The same document rendered by hand, as the encoder is obliged to render it.
fn nested_arrays_pretty(depth: usize, width: usize) -> String {
    let mut out = String::new();
    for level in 0..depth {
        out.push_str("[\n");
        out.push_str(&" ".repeat((level + 1) * width));
    }
    out.push('1');
    for level in (0..depth).rev() {
        out.push('\n');
        out.push_str(&" ".repeat(level * width));
        out.push(']');
    }
    out.push('\n');
    out
}

#[test]
fn nesting_deeper_than_the_static_fill_indents_by_repeated_chunks() {
    // The encoder splices indentation from a fixed 128-byte run of spaces. At
    // width 7 that runs out past depth 18, so this depth forces the chunked
    // write path AND the non-spliceable object-key fallback beside it.
    let depth = 40;
    assert_eq!(
        render(&nested_arrays(depth), Some(JsonIndent::Spaces(7))),
        nested_arrays_pretty(depth, 7)
    );
}
