//! Fact writes (`PATH.@comment = …`).
//!
//! `.port.@comment = ["x"]` lowers to a `FactAssign` plan node, the run
//! records fact deltas beside the unchanged document, and `--edit` applies
//! them as SPAN OPERATIONS against the retained source — replace, delete
//! (`= []`), or insert — then re-verifies by re-decoding the patched source
//! and comparing BOTH the value and the written node's comment fact.
//! Query-time (no `--edit`) applies the same deltas in memory: later
//! `.@comment` reads see the overlay, and encode publishes the document.
//!
//! The laws pinned here:
//! - a fact write compiles in ordinary mode;
//! - `.@bogus` writes stay rejected, while `.&attr` ATTRIBUTE writes compile
//!   and refuse cleanly over a format with no attribute grammar;
//! - dynamic selectors (`.@($r)`, `.&($h)`) and computed bases (`.[$k].@…`)
//!   compile and splice exactly like their static twins; an unknown DYNAMIC
//!   role is a runtime rejection naming the refusal;
//! - under `--edit` the write touches ONLY comment bytes;
//! - `|=` reads the current payload as `.`, `= []` deletes;
//! - a mixed fact+value program compiles and publishes both patches;
//!   a non-static fact path is a compile rejection.

/// A process-lifetime built-in dialect for request construction (123 X5).
fn json_dialect() -> &'static DialectId {
    Box::leak(Box::new(DialectId::try_new("rfc8259").expect("dialect")))
}

use jqf_codec_core::{DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
use jqf_data::{DialectId, FormatId};
use jqf_engine::{CodecRequirementPolicy, try_compile_program, try_compile_program_for_edit};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_sdk::{
    CodecCatalog, EncodedItemReport, FacadeFraming, Input, ItemSink, Outcome, PipelineError, PipelinePolicy, Report,
    Request,
};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

/// Local mirror of the drive-level edit outcome: the drive type is
/// crate-private, so tests reconstruct it from `Outcome` exactly as
/// request.rs does.
#[derive(Debug)]
// The mirror exists so `matches!` arms keep their names; not every variant
// is constructed or read by every suite.
#[allow(dead_code)]
enum EditRun {
    Completed(jqf_sdk::SequenceReport),
    Declined,
}

const COOPERATIVE_CREDITS: u32 = 64;
static CONTROL: ContinueControl = ContinueControl;

/// Collects every published byte and counts item boundaries.
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

fn toml_policy() -> PipelinePolicy<'static> {
    // The decode dialect names the request's own input dialect: the provider
    // factories dispatch on `policy.decode.dialect`, so it must match the
    // `with_format` pair the caller passes.
    let toml_dialect: &'static DialectId = Box::leak(Box::new(
        DialectId::try_new(jqf_codec_toml::TOML_1_0_DIALECT_ID).expect("dialect"),
    ));
    PipelinePolicy {
        decode: DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: toml_dialect,
            options: None,
            allow_adjacent_values: false,
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

/// Runs the edit lane in EDIT-MODE compilation (the one mode that lowers a
/// fact assignment), writing published bytes into `sink`.
fn run_fact_edit(
    input: &[u8],
    sink: &mut CollectingSink,
    program_source: &str,
) -> Result<EditRun, PipelineError<String>> {
    let registration = jqf_codec_toml::registration_1_0().expect("toml registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_toml::FORMAT_ID).expect("format id is valid");
    let input_dialect = || DialectId::try_new(jqf_codec_toml::TOML_1_0_DIALECT_ID).expect("input dialect is valid");
    let output_dialect =
        || DialectId::try_new(jqf_codec_toml::TOML_JQF_1_0_DIALECT_ID).expect("output dialect is valid");
    let mut resources = resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled = try_compile_program_for_edit(program_source, policy, &resources).expect("program compiles");
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.toml",
        input,
        0,
    );
    let request = Request::new(&compiled, Input::Whole(input))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(format(), input_dialect())
        .with_output_format(format(), output_dialect())
        .with_policy(toml_policy())
        .with_framing(FacadeFraming::item_suffix(b""))
        .with_resources(&mut resources)
        .editing();
    match jqf_sdk::execute(request, sink) {
        Ok(Outcome::Served(Report::Sequence(report))) => Ok(EditRun::Completed(report)),
        Ok(Outcome::Served(other)) => panic!("unexpected drive report: {other:?}"),
        Ok(Outcome::Declined) => Ok(EditRun::Declined),
        Err(error) => match error {
            jqf_sdk::Failure::Pipeline(error) => Err(error),
            other => panic!("unexpected failure class: {other:?}"),
        },
    }
}

/// Runs the fact-write lane over a strict-JSON document (the JSON registration
/// mirrors the TOML helper's shape, with the compact encode options the
/// edit-lane tests pin).
fn run_json_fact_edit(
    input: &[u8],
    sink: &mut CollectingSink,
    program_source: &str,
) -> Result<EditRun, PipelineError<String>> {
    let registration = jqf_codec_json::registration().expect("json registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid");
    let options: &'static jqf_codec_json::JsonEncodeOptions = Box::leak(Box::new(jqf_codec_json::JsonEncodeOptions {
        indent: jqf_codec_json::JsonIndent::Compact,
        raw_strings: false,
        sort_keys: false,
        ascii_output: false,
        raw_output_nul: false,
    }));
    let policy = PipelinePolicy {
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
        encode_options: Some(options as &(dyn core::any::Any + Send + Sync)),
        cooperative_credits: COOPERATIVE_CREDITS,
        split: None,

        max_iterations: None,
    };
    let mut resources = resources();
    let requirement = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled = try_compile_program_for_edit(program_source, requirement, &resources).expect("program compiles");
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
        .with_policy(policy)
        .with_framing(FacadeFraming::item_suffix(b""))
        .with_resources(&mut resources)
        .editing();
    match jqf_sdk::execute(request, sink) {
        Ok(Outcome::Served(Report::Sequence(report))) => Ok(EditRun::Completed(report)),
        Ok(Outcome::Served(other)) => panic!("unexpected drive report: {other:?}"),
        Ok(Outcome::Declined) => Ok(EditRun::Declined),
        Err(error) => match error {
            jqf_sdk::Failure::Pipeline(error) => Err(error),
            other => panic!("unexpected failure class: {other:?}"),
        },
    }
}

/// The TOML gate-1 shape: a commented value, replaced through `.@comment`.
#[test]
fn toml_fact_write_replaces_the_leading_block() {
    let mut sink = CollectingSink::new();
    let run = run_fact_edit(
        b"# server config\nport = 8080\nhost = \"localhost\"\n",
        &mut sink,
        ".port.@comment = [\"raised for the migration\"]",
    )
    .expect("a single-output fact edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(
        sink.bytes,
        b"# raised for the migration\nport = 8080\nhost = \"localhost\"\n"
    );
    assert_eq!(sink.items, 1);
}

/// The INLINE twin of the gate-1 shape (144 S4-T1): `.@comment_inline`
/// replaces the node's own-line trailing comment, leaving the leading block
/// above and the sibling below untouched.
#[test]
fn toml_fact_inline_write_replaces_the_own_line_trailing_gate1_shape() {
    let mut sink = CollectingSink::new();
    let run = run_fact_edit(
        b"# server config\nport = 8080 # main port\nhost = \"localhost\"\n",
        &mut sink,
        ".port.@comment_inline = [\"raised for the migration\"]",
    )
    .expect("a single-output fact edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    // The leading block survives; the trailing comment is replaced in place.
    assert_eq!(
        sink.bytes,
        b"# server config\nport = 8080 # raised for the migration\nhost = \"localhost\"\n"
    );
    assert_eq!(sink.items, 1);
}

/// The FOOT twin of the gate-1 shape (144 S4-T1): `.@comment_foot` replaces
/// the comment run below the section's block, leaving the leading block
/// above the header and the next section untouched.
#[test]
fn toml_fact_foot_write_replaces_the_section_foot() {
    let mut sink = CollectingSink::new();
    let run = run_fact_edit(
        b"# server config\n[server]\n  # main foot\n[database]\nuser = \"admin\"\n",
        &mut sink,
        ".server.@comment_foot = [\"raised for the migration\"]",
    )
    .expect("a single-output fact edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    // The leading block and the next section survive; the foot run is
    // replaced in place.
    assert_eq!(
        sink.bytes,
        b"# server config\n[server]\n  # raised for the migration\n[database]\nuser = \"admin\"\n"
    );
    assert_eq!(sink.items, 1);
}
/// allow-list already spelled `comment` (normalized at lowering), so the
/// alias write publishes the same bytes as the canonical spelling.
#[test]
fn comment_head_write_is_byte_identical_to_comment() {
    let input = b"# lead a\na = 1\n# lead b\nb = 2\n";
    let mut head_sink = CollectingSink::new();
    run_fact_edit(input, &mut head_sink, ".a.@comment_head = [\"new head\"]").expect("the alias write completes");
    let mut canonical_sink = CollectingSink::new();
    run_fact_edit(input, &mut canonical_sink, ".a.@comment = [\"new head\"]").expect("the canonical write completes");
    assert_eq!(
        head_sink.bytes, canonical_sink.bytes,
        "the alias write must publish the canonical write's bytes"
    );
    assert_eq!(head_sink.bytes, b"# new head\na = 1\n# lead b\nb = 2\n");
    assert_eq!(head_sink.items, 1);
}

/// The gate's own-line trailing comment: TOML attaches a statement's
/// trailing comment to the statement's value, but the trailing is the INLINE
/// position's to own — a leading write replaces the block above and never
/// touches the node's own-line inline comment (144 S4-T1, the
/// `trailing_owned` drop).
#[test]
fn toml_fact_write_leaves_the_own_line_trailing_comment() {
    let mut sink = CollectingSink::new();
    let run = run_fact_edit(
        b"# server config\nport = 8080 # main port\nhost = \"localhost\"\n",
        &mut sink,
        ".port.@comment = [\"raised for the migration\"]",
    )
    .expect("a single-output fact edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    // The leading block is replaced; the inline trailing comment (the INLINE
    // position's, addressed by `.port.@comment_inline`) survives verbatim.
    assert_eq!(
        sink.bytes,
        b"# raised for the migration\nport = 8080 # main port\nhost = \"localhost\"\n"
    );
    assert_eq!(sink.items, 1);
}

/// The INLINE position dispatches on the delta's role (the lane-I1 seam
/// generalization): `.@comment_inline` replaces the node's OWN-LINE trailing
/// comment, leaving the leading block untouched.
#[test]
fn toml_fact_inline_write_replaces_the_own_line_trailing_comment() {
    let mut sink = CollectingSink::new();
    let run = run_fact_edit(
        b"# lead\nport = 8080 # main port\n",
        &mut sink,
        ".port.@comment_inline = [\"note\"]",
    )
    .expect("an inline fact edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    // The leading block survives; the trailing comment is replaced.
    assert_eq!(sink.bytes, b"# lead\nport = 8080 # note\n");
    assert_eq!(sink.items, 1);
}

/// An inline write with no existing trailing comment INSERTS one at the
/// value's line end.
#[test]
fn toml_fact_inline_write_inserts_when_none_exists() {
    let mut sink = CollectingSink::new();
    let run = run_fact_edit(b"port = 8080\n", &mut sink, ".port.@comment_inline = [\"note\"]")
        .expect("an inline insert completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(sink.bytes, b"port = 8080 # note\n");
}

/// `|=` reads the current payload as `.`, so an append lands beside the old
/// lines; `= []` removes the whole block.
#[test]
fn toml_fact_append_and_delete() {
    let mut sink = CollectingSink::new();
    let run = run_fact_edit(
        b"# one\n# two\nport = 8080\n",
        &mut sink,
        ".port.@comment |= . + [\"three\"]",
    )
    .expect("an append fact edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(sink.bytes, b"# one\n# two\n# three\nport = 8080\n");

    let mut sink = CollectingSink::new();
    let run =
        run_fact_edit(b"# one\nport = 8080\n", &mut sink, ".port.@comment = []").expect("a delete fact edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(sink.bytes, b"port = 8080\n");
}

/// A missing value path writes nothing and the document passes through.
#[test]
fn toml_fact_write_missing_path_is_a_no_op() {
    let mut sink = CollectingSink::new();
    let run =
        run_fact_edit(b"port = 8080\n", &mut sink, ".missing.@comment = [\"x\"]").expect("a no-op fact edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(sink.bytes, b"port = 8080\n");
}

/// A non-list payload is refused (the comment fact's own shape is a list of
/// text lines): the edit lane fails with a representation refusal rather than
/// emitting wrong bytes.
#[test]
fn toml_fact_write_non_list_payload_is_a_refusal() {
    let mut sink = CollectingSink::new();
    let run = run_fact_edit(b"port = 8080\n", &mut sink, ".port.@comment = 5");
    let error = run.expect_err("a fact edit with a non-list payload must fail");
    let jqf_sdk::PipelineFailure::Codec(codec) = error.failure() else {
        panic!("expected a codec failure, got {error:?}");
    };
    assert_eq!(
        codec.kind(),
        jqf_codec_core::CodecFailureKind::UnsupportedRepresentation
    );
}

/// A lone STRING payload denotes the one-line list
/// the read side would round-trip (`"raised"` == `["raised"]`), so it is
/// coerced at the fact delta's record site instead of refused. The comment
/// fact's canonical shape is a list of text lines; a scalar comment has
/// exactly one meaning.
#[test]
fn toml_fact_write_lone_string_is_coerced_to_a_list() {
    let mut sink = CollectingSink::new();
    let run = run_fact_edit(
        b"# server config\nport = 8080\n",
        &mut sink,
        ".port.@comment = \"raised for the migration\"",
    )
    .expect("a lone-string fact edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(sink.bytes, b"# raised for the migration\nport = 8080\n");
    assert_eq!(sink.items, 1);
}

/// A string carrying line breaks SPLITS into
/// the list of lines it denotes (`"a\\nb"` == `["a", "b"]`) rather than
/// rendering its first line only. The former would fail the edit lane's
/// verify re-decode (the read-back list could never equal the truncated
/// payload); the split round-trips by construction.
#[test]
fn toml_fact_write_multiline_string_splits_into_lines() {
    let mut sink = CollectingSink::new();
    let run = run_fact_edit(
        b"port = 8080\n",
        &mut sink,
        ".port.@comment = \"line one\\nline two\\nline three\"",
    )
    .expect("a multi-line string fact edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(sink.bytes, b"# line one\n# line two\n# line three\nport = 8080\n");
    assert_eq!(sink.items, 1);
}

/// The same line-split law applied to a LIST item carrying a line break: an
/// item is one line of the comment, so an embedded `\\n` names two lines.
#[test]
fn toml_fact_write_multiline_list_item_splits_into_lines() {
    let mut sink = CollectingSink::new();
    let run = run_fact_edit(b"port = 8080\n", &mut sink, ".port.@comment = [\"a\\nb\"]")
        .expect("a multi-line list-item fact edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(sink.bytes, b"# a\n# b\nport = 8080\n");
    assert_eq!(sink.items, 1);
}

/// The YAML gate: a node inside a nested mapping, at the member's own indent.
/// A JSON document cannot carry a comment fact at all (the strict codec
/// retains no comment syntax), so a fact write over one is a CLEAN codec
/// refusal, never the internal-contract class the contract law declares
/// unreachable from user input — on a container target (no retained span)
/// and a leaf target alike.
#[test]
fn json_fact_write_is_a_clean_refusal() {
    for (input, program) in [
        (b"{\"a\":{\"href\":\"h\"}}".as_slice(), ".a.@comment = [\"x\"]"),
        (b"{\"a\":{\"href\":\"h\"}}".as_slice(), ".a.href.@comment = [\"x\"]"),
        // Plan 185's new spellings ride the same format fact: the role/kind is
        // writable grammar-wise, and JSON declares no comment role, so the
        // seam refuses cleanly whatever spelling named it.
        (
            b"{\"a\":{\"href\":\"h\"}}".as_slice(),
            "\"comment\" as $r | .a.@($r) = [\"x\"]",
        ),
        (
            b"{\"a\":{\"href\":\"h\"}}".as_slice(),
            "\"a\" as $t | .[$t].href.@comment = [\"x\"]",
        ),
    ] {
        let mut sink = CollectingSink::new();
        let run = run_json_fact_edit(input, &mut sink, program);
        let error = run.expect_err("a JSON fact write must refuse cleanly");
        let jqf_sdk::PipelineFailure::Codec(codec) = error.failure() else {
            panic!("expected a codec failure, got {error:?}");
        };
        assert_eq!(
            codec.kind(),
            jqf_codec_core::CodecFailureKind::UnsupportedRepresentation,
            "{program}: {codec:?}"
        );
        assert_eq!(sink.bytes.len(), 0, "a refused fact write publishes nothing");
    }
}
/// A fact write compiled in ORDINARY mode applies in memory (the query-time
/// lane). `--edit` is the splice into retained source, not a compile gate.
#[test]
fn fact_write_without_edit_mode_compiles() {
    let resources = resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled = try_compile_program(".port.@comment = [\"x\"]", policy, &resources)
        .expect("a fact write without --edit must compile");
    assert!(
        compiled.fact_writes(),
        "the ordinary compile must lower to a FactAssign"
    );
}

/// A non-admitted accessor write stays rejected even under edit-mode
/// compilation (a `.@bogus` selector names no writable fact role), while a
/// `.&attr` ATTRIBUTE write now compiles to a [`FactAssign`] with the
/// markup attribute role (lane I1's assignment surface for markup
/// accessors). The four METADATA roles (`style`, `tag`, `anchor`, `alias`)
/// are admitted by the grammar (145 C5) and refused or served later, in the
/// codec seam, per the encode-or-report-a-loss law.
#[test]
fn non_admitted_writes_stay_rejected_and_attribute_writes_compile_under_edit() {
    let resources = resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let error = try_compile_program_for_edit(".port.@bogus = \"x\"", policy, &resources)
        .expect_err("a non-admitted accessor write must not compile");
    let rendered = format!("{error}");
    assert!(
        rendered.contains("assignment/update over a node or attribute accessor"),
        "{rendered}"
    );
    let attribute = try_compile_program_for_edit(".port.&href = \"x\"", policy, &resources)
        .expect("an attribute write must compile under edit mode");
    assert!(
        attribute.fact_writes(),
        "the attribute write must lower to a FactAssign"
    );
}

/// An attribute write over a format with no attribute grammar refuses CLEANLY
/// in the seam: TOML declares no attribute role, so `.port.&href = "x"`
/// compiles, records its delta, and the edit lane refuses with a
/// representation failure and zero published bytes — never wrong bytes.
#[test]
fn attribute_write_over_a_format_without_attribute_grammar_is_a_clean_refusal() {
    let mut sink = CollectingSink::new();
    let run = run_fact_edit(b"[server]\nhost = \"localhost\"\n", &mut sink, ".server.&href = \"x\"");
    let error = run.expect_err("a TOML attribute write must refuse cleanly");
    let jqf_sdk::PipelineFailure::Codec(codec) = error.failure() else {
        panic!("expected a codec failure, got {error:?}");
    };
    assert_eq!(
        codec.kind(),
        jqf_codec_core::CodecFailureKind::UnsupportedRepresentation,
        "{codec:?}"
    );
    assert_eq!(sink.bytes.len(), 0, "a refused attribute write publishes nothing");
}

/// A mixed fact+value program compiles and publishes both patches against the
/// original source. Constant writes commute: both source orders emit the same
/// bytes.
#[test]
fn mixed_fact_and_value_assignment_publishes_both() {
    let input = b"# server config\nport = 8080\nhost = \"localhost\"\n";
    let expected = b"# server config\nport = 9090\n# x\nhost = \"localhost\"\n";
    for program in [
        ".port = 9090 | .host.@comment = [\"x\"]",
        ".host.@comment = [\"x\"] | .port = 9090",
    ] {
        let mut sink = CollectingSink::new();
        let run = run_fact_edit(input, &mut sink, program).expect("a mixed leaf edit completes");
        assert!(matches!(run, EditRun::Completed(_)), "{program}");
        assert_eq!(sink.bytes, expected, "{program}");
        assert_eq!(sink.items, 1, "{program}");
    }
}

/// Same-node value + HEAD comment: the number span and the leading comment
/// block are disjoint, so both land.
#[test]
fn mixed_same_node_value_and_comment() {
    let mut sink = CollectingSink::new();
    let run = run_fact_edit(
        b"# server config\nport = 8080\nhost = \"localhost\"\n",
        &mut sink,
        ".port = 9090 | .port.@comment = [\"raised\"]",
    )
    .expect("a same-node mixed edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(sink.bytes, b"# raised\nport = 9090\nhost = \"localhost\"\n");
    assert_eq!(sink.items, 1);
}

/// Source order is the engine's: a fact RHS that reads a prior value write
/// sees the new value; the reverse sees the original.
#[test]
fn mixed_fact_rhs_reads_prior_value_write() {
    let input = b"port = 8080\nhost = \"localhost\"\n";
    let mut after_value = CollectingSink::new();
    run_fact_edit(
        input,
        &mut after_value,
        ".port = 9090 | .host.@comment = [(.port | tostring)]",
    )
    .expect("value-then-fact completes");
    assert_eq!(after_value.bytes, b"port = 9090\n# 9090\nhost = \"localhost\"\n");
    let mut after_fact = CollectingSink::new();
    run_fact_edit(
        input,
        &mut after_fact,
        ".host.@comment = [(.port | tostring)] | .port = 9090",
    )
    .expect("fact-then-value completes");
    assert_eq!(after_fact.bytes, b"port = 9090\n# 8080\nhost = \"localhost\"\n");
}

/// A structural value write has no leaf patches, and a whole re-encode would
/// drop the new facts, so mixed+structural refuses.
#[test]
fn mixed_structural_value_and_fact_is_refused() {
    let mut sink = CollectingSink::new();
    let error = run_fact_edit(
        b"port = 8080\nhost = \"localhost\"\n",
        &mut sink,
        ".port = {a: 1} | .host.@comment = [\"x\"]",
    )
    .expect_err("mixed structural must refuse");
    let jqf_sdk::PipelineFailure::Codec(codec) = error.failure() else {
        panic!("expected a codec failure, got {error:?}");
    };
    assert_eq!(
        codec.kind(),
        jqf_codec_core::CodecFailureKind::UnsupportedRepresentation,
        "{codec:?}"
    );
    assert_eq!(sink.bytes.len(), 0, "a refused mixed edit publishes nothing");
}

/// Two colliding FACT writes are ordinary input: the same clean refusal the
/// mixed fact+value twin gets, never a machine fault. The port foot insert and
/// the host head insert both claim the `host` line's start offset, so the
/// patch set is ambiguous — refused with prose and zero published bytes.
#[test]
fn colliding_fact_only_writes_are_a_clean_refusal() {
    let input = b"port = 8080\nhost = \"localhost\"\n";
    let mut sink = CollectingSink::new();
    let error = run_fact_edit(
        input,
        &mut sink,
        ".port.@comment_foot = [\"f\"] | .host.@comment_head = [\"h\"]",
    )
    .expect_err("colliding fact writes must refuse");
    let jqf_sdk::PipelineFailure::Codec(codec) = error.failure() else {
        panic!("expected a codec failure, got {error:?}");
    };
    assert_eq!(
        codec.kind(),
        jqf_codec_core::CodecFailureKind::UnsupportedRepresentation,
        "{codec:?}"
    );
    assert_eq!(sink.bytes.len(), 0, "a refused fact edit publishes nothing");
}

/// A fact target that is not a static key/index path is rejected.
#[test]
fn non_static_fact_path_is_rejected() {
    let resources = resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let error = try_compile_program_for_edit(".port[].@comment = [\"x\"]", policy, &resources)
        .expect_err("a non-static fact path must not compile");
    let rendered = format!("{error}");
    assert!(rendered.contains("non-static path"), "{rendered}");
}

/// Plan 185 lane 1: a dynamic role selector lowers like its static twin and
/// splices byte-identically (`"comment" as $r | .port.@($r) = …` ≡
/// `.port.@comment = …`). The role vocabulary is validated at RUN time; a
/// hole-free string selector folds static, so it shares the compile-time law.
#[test]
fn dynamic_role_write_matches_its_static_twin() {
    let input = b"# server config\nport = 8080\nhost = \"localhost\"\n";
    let expected = b"# raised\nport = 8080\nhost = \"localhost\"\n";
    let mut dynamic = CollectingSink::new();
    let run = run_fact_edit(input, &mut dynamic, "\"comment\" as $r | .port.@($r) = [\"raised\"]")
        .expect("the dynamic-role edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(dynamic.bytes, expected);
    assert_eq!(dynamic.items, 1);
    let mut folded = CollectingSink::new();
    run_fact_edit(input, &mut folded, ".port.@(\"comment\") = [\"raised\"]").expect("the folded-string edit completes");
    assert_eq!(folded.bytes, expected, "a hole-free selector folds static");
}

/// Lane 1 over the attribute channel: `.&($kind)` resolves its kind at run
/// time and splices exactly like the static `.&href` twin (XML serves the
/// attribute role).
#[test]
fn xml_dynamic_attribute_write_matches_its_static_twin() {
    let input = b"<r href=\"old\">t</r>";
    let expected = b"<r href=\"new\">t</r>";
    let mut dynamic = CollectingSink::new();
    let run = run_xml_fact_edit(input, &mut dynamic, "\"href\" as $h | .&($h) = \"new\"")
        .expect("the dynamic-attribute edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(dynamic.bytes, expected);
    let mut folded = CollectingSink::new();
    run_xml_fact_edit(input, &mut folded, r#".&["href"] = "new""#).expect("the folded-attribute edit completes");
    assert_eq!(folded.bytes, expected);
}

/// An unknown DYNAMIC role is a runtime rejection naming the refusal — never
/// a silent write, even when the value path would have missed.
#[test]
fn unknown_dynamic_role_raises_at_runtime() {
    let mut sink = CollectingSink::new();
    let error = run_fact_edit(b"port = 8080\n", &mut sink, "\"bogus\" as $r | .port.@($r) = [\"x\"]")
        .expect_err("an unknown dynamic role must raise");
    let jqf_sdk::PipelineFailure::Raised(raised) = error.failure() else {
        panic!("expected a raised rejection, got {error:?}");
    };
    let message = jqf_engine::raised_body(raised.value()).expect("message renders");
    assert!(
        message.contains("unknown fact write role") && message.contains("bogus"),
        "{message}"
    );
    // A missing PATH never silences the unknown-role rejection either:
    // validation happens before the walk.
    let mut missed = CollectingSink::new();
    let error = run_fact_edit(
        b"port = 8080\n",
        &mut missed,
        "\"bogus\" as $r | .missing.@($r) = [\"x\"]",
    )
    .expect_err("the unknown role must raise before the path walk");
    assert!(matches!(error.failure(), jqf_sdk::PipelineFailure::Raised(_)));
    assert_eq!(missed.bytes.len(), 0);
}

/// Plan 185 lane 2: a computed base splices exactly like its static twin
/// (`"server" as $t | .[$t].host.@comment = …` ≡ `.server.host.@comment = …`).
#[test]
fn computed_base_fact_write_splices_like_the_static_twin() {
    let input = b"[server]\nhost = \"localhost\"\n";
    let expected = b"[server]\n# raised\nhost = \"localhost\"\n";
    let mut computed = CollectingSink::new();
    let run = run_fact_edit(
        input,
        &mut computed,
        "\"server\" as $t | .[$t].host.@comment = [\"raised\"]",
    )
    .expect("the computed-base edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(computed.bytes, expected);
    assert_eq!(computed.items, 1);
    let mut statique = CollectingSink::new();
    run_fact_edit(input, &mut statique, ".server.host.@comment = [\"raised\"]").expect("the static twin completes");
    assert_eq!(statique.bytes, expected);
}

/// Multi-output targets fan out per output, in order — one binder emitting two
/// keys runs the whole write twice against the original document. The engine's
/// per-output law matches jq; the EDIT lane's own exactly-one-output law then
/// refuses the two outputs (the same law that refuses a multi-output VALUE
/// assignment), so nothing splices and zero bytes publish.
#[test]
fn multi_output_computed_base_fans_out_per_output() {
    let mut sink = CollectingSink::new();
    let error = run_fact_edit(
        b"[server]\nhost = \"h\"\nuser = \"u\"\n",
        &mut sink,
        "(\"host\", \"user\") as $k | .server[$k].@comment = [\"c\"]",
    )
    .expect_err("two fan-out outputs must hit the edit lane's output-count law");
    match error.failure() {
        jqf_sdk::PipelineFailure::EditOutputCount { observed } => {
            assert_eq!(*observed, 2, "one write per path output");
        }
        other => panic!("expected the output-count refusal, got {other:?}"),
    }
    assert_eq!(sink.bytes.len(), 0, "nothing published for a refused edit");
}

/// The XML comment-write round-trip (145 I4): XML declares ONE comment
/// position — the element's comment CHILDREN, served by `.@comment` — and
/// its codec renders the write in XML's own `<!-- … -->` syntax, replacing
/// each comment child's authored span. The round-trip write (the payload
/// equals the element's existing comment children) publishes the source
/// byte-identically, because the comments are VALUE children and the
/// fact-write lane is value-identity by law.
#[test]
fn xml_fact_write_round_trips_the_comment_children() {
    let mut sink = CollectingSink::new();
    let run = run_xml_fact_edit(
        b"<r><!-- lead --><a>1</a><!-- trail --></r>",
        &mut sink,
        ".@comment = [\" lead \", \" trail \"]",
    )
    .expect("a single-output fact edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(sink.bytes, b"<r><!-- lead --><a>1</a><!-- trail --></r>");
    assert_eq!(sink.items, 1);
}

/// A missing `.&name` inserts ` name="…"` immediately before the start-tag
/// close: after existing attributes on an open tag, and before `/>` on a
/// self-closing tag. The new attribute is always double-quoted; `&` is
/// attribute-escaped.
#[test]
fn xml_missing_attribute_inserts_before_the_start_tag_close() {
    let mut open = CollectingSink::new();
    let run =
        run_xml_fact_edit(b"<r a=\"1\">t</r>", &mut open, ".&id = \"x\"").expect("insert on an open tag completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(open.bytes, b"<r a=\"1\" id=\"x\">t</r>");

    let mut empty = CollectingSink::new();
    run_xml_fact_edit(b"<r/>", &mut empty, ".&id = \"x\"").expect("insert on a self-closing tag completes");
    assert_eq!(empty.bytes, b"<r id=\"x\"/>");

    let mut escaped = CollectingSink::new();
    run_xml_fact_edit(b"<r a='1'>t</r>", &mut escaped, ".&b = \"c & d\"")
        .expect("insert next to a single-quoted sibling completes");
    assert_eq!(escaped.bytes, b"<r a='1' b=\"c &amp; d\">t</r>");
}

/// An existing `.&name` still rewrites the one quoted value span; siblings
/// and the rest of the start tag stay put.
#[test]
fn xml_existing_attribute_rewrite_patches_one_span() {
    let mut sink = CollectingSink::new();
    run_xml_fact_edit(b"<r a=\"1\">t</r>", &mut sink, ".&a = \"2\"").expect("an existing attribute rewrite completes");
    assert_eq!(sink.bytes, b"<r a=\"2\">t</r>");
}

fn xml_attribute_refusal_message(input: &[u8], program: &str) -> String {
    let mut sink = CollectingSink::new();
    let error = run_xml_fact_edit(input, &mut sink, program).expect_err("the attribute write must refuse");
    assert_eq!(sink.bytes.len(), 0, "a refused attribute write publishes nothing");
    let jqf_sdk::PipelineFailure::Codec(codec) = error.failure() else {
        panic!("expected a codec failure, got {error:?}");
    };
    codec
        .diagnostic()
        .map_or_else(|| format!("{codec:?}"), |diagnostic| diagnostic.message().to_owned())
}

/// A non-string payload stays the single-text refusal; `null` stays the
/// documented deletion refusal. Neither path inserts.
#[test]
fn xml_attribute_write_refuses_non_text_and_null() {
    let number = xml_attribute_refusal_message(b"<r/>", ".&id = 1");
    assert!(
        number.contains("single text value"),
        "number payload must stay the single-text refusal: {number}"
    );
    let null = xml_attribute_refusal_message(b"<r/>", ".&id = null");
    assert!(
        null.contains("delete"),
        "null payload must stay the deletion refusal: {null}"
    );
}

/// Clark `{uri}local`, a prefixed `QName`, and `xmlns` are not insertable
/// names. The write refuses; it does not splice a namespaced token.
#[test]
fn xml_attribute_insert_refuses_namespaced_kind() {
    let clark = xml_attribute_refusal_message(b"<r/>", r#".&["{http://example.com}id"] = "x""#);
    assert!(
        clark.contains("namespaced") || clark.contains("unprefixed"),
        "clark notation must refuse: {clark}"
    );
    let prefixed = xml_attribute_refusal_message(b"<r/>", r#".&["foo:bar"] = "x""#);
    assert!(
        prefixed.contains("namespaced") || prefixed.contains("unprefixed"),
        "a prefixed name must refuse: {prefixed}"
    );
    let xmlns = xml_attribute_refusal_message(b"<r/>", r#".&xmlns = "x""#);
    assert!(xmlns.contains("xmlns"), "xmlns must refuse: {xmlns}");
}

/// Runs the fact-write lane over an XML document (the XML registration
/// mirrors the TOML helper's shape; the source-profile output dialect is
/// the byte-faithful echo the edit lane publishes through).
fn run_xml_fact_edit(
    input: &[u8],
    sink: &mut CollectingSink,
    program_source: &str,
) -> Result<EditRun, PipelineError<String>> {
    let registration = jqf_codec_xml::registration().expect("xml registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_xml::FORMAT_ID).expect("format id is valid");
    let input_dialect = || DialectId::try_new(jqf_codec_xml::XML_DOCUMENT_DIALECT_ID).expect("input dialect is valid");
    let output_dialect = || DialectId::try_new(jqf_codec_xml::XML_SOURCE_DIALECT_ID).expect("output dialect is valid");
    let mut resources = resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled = try_compile_program_for_edit(program_source, policy, &resources).expect("program compiles");
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.xml",
        input,
        0,
    );
    let request = Request::new(&compiled, Input::Whole(input))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(format(), input_dialect())
        .with_output_format(format(), output_dialect())
        .with_policy(xml_policy())
        .with_framing(FacadeFraming::item_suffix(b""))
        .with_resources(&mut resources)
        .editing();
    match jqf_sdk::execute(request, sink) {
        Ok(Outcome::Served(Report::Sequence(report))) => Ok(EditRun::Completed(report)),
        Ok(Outcome::Served(other)) => panic!("unexpected drive report: {other:?}"),
        Ok(Outcome::Declined) => Ok(EditRun::Declined),
        Err(error) => match error {
            jqf_sdk::Failure::Pipeline(error) => Err(error),
            other => panic!("unexpected failure class: {other:?}"),
        },
    }
}

fn xml_policy() -> PipelinePolicy<'static> {
    // The decode dialect names the request's own input dialect: the provider
    // factories dispatch on `policy.decode.dialect`, so it must match the
    // `with_format` pair the caller passes.
    let xml_dialect: &'static DialectId = Box::leak(Box::new(
        DialectId::try_new(jqf_codec_xml::XML_DOCUMENT_DIALECT_ID).expect("dialect"),
    ));
    PipelinePolicy {
        decode: DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: xml_dialect,
            options: None,
            allow_adjacent_values: false,
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
