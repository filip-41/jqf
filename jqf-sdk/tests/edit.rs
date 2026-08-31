//! The source-preserving EDIT lane — SDK coverage.
//!
//! `execute_source_edit` runs the program per input document and publishes the
//! whole EDITED document per document: untouched bytes come from the retained
//! source, edited spans are re-encoded. The lane is governed by three laws:
//!
//! 1. EXACTLY ONE output per document — zero or multiple outputs is an error,
//!    and the failing document publishes nothing (earlier documents' prefix
//!    stands);
//! 2. any doubt falls back to the floor — a patch that cannot be proven
//!    sound re-encodes the whole document, never wrong bytes;
//! 3. the SDK publishes patched document bytes with NO facade framing; the
//!    host owns separators.

/// A process-lifetime built-in dialect for request construction (123 X5).
fn json_dialect() -> &'static DialectId {
    Box::leak(Box::new(DialectId::try_new("rfc8259").expect("dialect")))
}

use jqf_codec_core::{DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
use jqf_data::{DialectId, FormatId};
use jqf_engine::{CodecRequirementPolicy, CompileOptions, try_compile_program};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_sdk::{
    CodecCatalog, EncodedItemReport, FacadeFraming, Input, ItemSink, Outcome, PipelineError, PipelineFailure,
    PipelinePolicy, Report, Request,
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

/// Compact encoding options: the SDK tests pin exact patched bytes, so the
/// edited spans render compactly and expectations stay deterministic.
fn compact_policy() -> PipelinePolicy<'static> {
    let options: &'static jqf_codec_json::JsonEncodeOptions = Box::leak(Box::new(jqf_codec_json::JsonEncodeOptions {
        indent: jqf_codec_json::JsonIndent::Compact,
        raw_strings: false,
        sort_keys: false,
        ascii_output: false,
        raw_output_nul: false,
    }));
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
        encode_options: Some(options as &(dyn core::any::Any + Send + Sync)),
        cooperative_credits: COOPERATIVE_CREDITS,
        split: None,

        max_iterations: None,
    }
}

/// Runs the edit lane over `input`, writing published bytes into `sink`.
fn run_edit(input: &[u8], sink: &mut CollectingSink, program_source: &str) -> Result<EditRun, PipelineError<String>> {
    let registration = jqf_codec_json::registration().expect("json registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid");
    let mut resources = resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled =
        try_compile_program(program_source, policy, CompileOptions::new(), &resources).expect("program compiles");
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
        .with_policy(compact_policy())
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

/// A member appended to a NESTED pretty container lands at the last
/// member's indentation: the run between the last value and the closer
/// ends in the CLOSER's column (parent-level whitespace), and composing it
/// with the member indent used to over-indent once per nesting depth.
#[test]
fn nested_append_lands_at_member_indent() {
    let mut sink = CollectingSink::new();
    let run = run_edit(b"{\n  \"outer\": {\n    \"a\": 1\n  }\n}\n", &mut sink, ".outer.b = 2")
        .expect("a single-output edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    // The item framing carries no suffix here, so the input's trailing
    // newline (outside the document span) is not re-emitted.
    assert_eq!(sink.bytes, b"{\n  \"outer\": {\n    \"a\": 1,\n    \"b\": 2\n  }\n}");
}

/// The JSONC twin of `nested_append_lands_at_member_indent`.
#[test]
fn jsonc_nested_append_lands_at_member_indent() {
    let mut sink = CollectingSink::new();
    let run = run_jsonc_edit(b"{\n  \"outer\": {\n    \"a\": 1\n  }\n}\n", &mut sink, ".outer.b = 2")
        .expect("a single-output edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(sink.bytes, b"{\n  \"outer\": {\n    \"a\": 1,\n    \"b\": 2\n  }\n}\n");
}

#[test]
fn replacement_patches_only_the_edited_span() {
    let mut sink = CollectingSink::new();
    let run = run_edit(b"{\"a\":1,\"b\":[1,2]}", &mut sink, ".b = [3]").expect("a single-output edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(sink.bytes, b"{\"a\":1,\"b\":[3]}");
    assert_eq!(sink.items, 1);
}

#[test]
fn nested_replacement_preserves_untouched_presentation() {
    let mut sink = CollectingSink::new();
    let run = run_edit(b"{\"a\":{\"b\" : 1},\"c\":2}", &mut sink, ".a.b = 9").expect("a single-output edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    // The untouched `"b" : ` spelling and the outer structure survive; only the
    // leaf span changes.
    assert_eq!(sink.bytes, b"{\"a\":{\"b\" : 9},\"c\":2}");
}

#[test]
fn untouched_string_leaf_past_an_array_of_objects_is_not_patched() {
    // The strict codec records a string VALUE's source span as its INNER
    // content (between the quotes). The leaf diff must compare that inner text
    // against the decoded value — comparing it against the ENCODED bytes
    // instead saw every unchanged string as changed and emitted a corrupting
    // patch that wrapped the string in extra quotes (`"a"` -> `""a""`),
    // breaking the edit lane on any document whose path scans past an
    // array-of-objects. The `name` string here is untouched: it must survive
    // verbatim while the deep number leaf is patched.
    let mut sink = CollectingSink::new();
    let run = run_edit(
        b"{\"catalog\":[{\"id\":1,\"name\":\"a\"}],\"meta\":{\"n\":1}}",
        &mut sink,
        ".meta.n = 2",
    )
    .expect("a single-output edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(
        sink.bytes,
        b"{\"catalog\":[{\"id\":1,\"name\":\"a\"}],\"meta\":{\"n\":2}}"
    );
    assert_eq!(sink.items, 1);
}

#[test]
fn edited_string_leaf_replaces_the_whole_quoted_span() {
    // A CHANGED string leaf must replace the entire quoted span, not the inner
    // text: patching the inner bytes with the encoded value produced `""z""`.
    let mut sink = CollectingSink::new();
    let run = run_edit(
        b"{\"catalog\":[{\"id\":1,\"name\":\"a\"}],\"meta\":{\"n\":1}}",
        &mut sink,
        ".catalog[0].name = \"z\"",
    )
    .expect("a single-output edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(
        sink.bytes,
        b"{\"catalog\":[{\"id\":1,\"name\":\"z\"}],\"meta\":{\"n\":1}}"
    );
    assert_eq!(sink.items, 1);
}

#[test]
fn escaped_string_leaf_takes_the_floor_not_a_corrupting_patch() {
    // An ESCAPED string is STORED by the codec (it must be decoded), so its
    // node has no retained source span — but 55247d153's source-backed edit
    // lane keeps the authored bytes of a leaf the program did NOT touch:
    // `.y = 2` leaves `.x` alone, so `a\u0041` survives VERBATIM instead of
    // forcing a whole-document re-encode that would normalize it to `aA`.
    // The assertion is that the untouched leaf's spelling is preserved —
    // never a corrupting inner-span patch, and never needless re-encode
    // churn.
    let mut sink = CollectingSink::new();
    let run = run_edit(b"{\"x\":\"a\\u0041\",\"y\":1}", &mut sink, ".y = 2").expect("a single-output edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(sink.bytes, b"{\"x\":\"a\\u0041\",\"y\":2}");
    assert_eq!(sink.items, 1);
}

#[test]
fn insertion_reencodes_the_parent_container() {
    let mut sink = CollectingSink::new();
    let run = run_edit(b"{\"a\":1}", &mut sink, ".c = 4").expect("a single-output edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(sink.bytes, b"{\"a\":1,\"c\":4}");
}

#[test]
fn deletion_reencodes_the_parent_container() {
    let mut sink = CollectingSink::new();
    let run = run_edit(b"{\"a\":1,\"b\":2}", &mut sink, "del(.b)").expect("a single-output edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(sink.bytes, b"{\"a\":1}");
}

#[test]
fn adjacent_deletions_fall_to_the_floor_instead_of_failing() {
    // Removing a container's FIRST member and the member after it names the
    // same comma twice: the first member's cut runs through the FOLLOWING
    // comma, the next member's from the PRECEDING one. The splice's declared
    // law is that any doubt falls to the whole-document floor, and an
    // overlapping cut set is doubt — it used to fail the request outright,
    // leaving the file untouched with a contract-violation diagnostic.
    for (input, program, expected) in [
        (&b"{\"a\":1,\"b\":2,\"c\":3}"[..], "del(.a,.b)", &b"{\"c\":3}"[..]),
        (b"[1,2,3]", "del(.[0],.[1])", b"[3]"),
        (b"{\"a\":1,\"b\":2}", "del(.a,.b)", b"{}"),
        // The non-adjacent cuts still splice, and agree with the floor.
        (b"{\"a\":1,\"b\":2,\"c\":3}", "del(.a,.c)", b"{\"b\":2}"),
    ] {
        let mut sink = CollectingSink::new();
        let run = run_edit(input, &mut sink, program).expect("a single-output edit completes");
        assert!(matches!(run, EditRun::Completed(_)), "{program} on {input:?}");
        assert_eq!(sink.bytes, expected, "{program} on {input:?}");
    }
}

#[test]
fn non_assignment_program_echoes_the_document() {
    // `--edit`'s output subject is the document, so a program that reads but
    // does not assign publishes the unchanged document bytes verbatim.
    let mut sink = CollectingSink::new();
    let run = run_edit(b"{ \"a\" : 1 }", &mut sink, ".a").expect("a single-output edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(sink.bytes, b"{ \"a\" : 1 }");
}

#[test]
fn multi_output_program_errors_without_publishing() {
    let mut sink = CollectingSink::new();
    let error = run_edit(b"{}", &mut sink, ".a = (1,2)").expect_err("a multi-output edit must fail the document");
    assert!(
        matches!(error.failure(), PipelineFailure::EditOutputCount { observed: 2 }),
        "the failure must be a dedicated edit error, got {error:?}"
    );
    assert!(sink.bytes.is_empty(), "a failing document publishes nothing");
    assert_eq!(sink.items, 0);
}

#[test]
fn zero_output_program_errors_without_publishing() {
    let mut sink = CollectingSink::new();
    let error = run_edit(b"{}", &mut sink, "empty").expect_err("a zero-output edit must fail the document");
    assert!(
        matches!(error.failure(), PipelineFailure::EditOutputCount { observed: 0 }),
        "the failure must be a dedicated edit error, got {error:?}"
    );
    assert!(sink.bytes.is_empty());
}

#[test]
fn adjacent_documents_are_each_edited() {
    let mut sink = CollectingSink::new();
    let run = run_edit(b"{\"a\":1} {\"a\":2}", &mut sink, ".a = 9").expect("per-document edits complete");
    assert!(matches!(run, EditRun::Completed(_)));
    // The SDK publishes patched document bytes with NO facade separators; the
    // host owns framing between documents.
    assert_eq!(sink.bytes, b"{\"a\":9}{\"a\":9}");
    assert_eq!(sink.items, 2);
}

#[test]
fn later_document_failure_keeps_the_prefix_and_errors() {
    let mut sink = CollectingSink::new();
    let error = run_edit(b"{\"a\":1} {\"x\":5}", &mut sink, ".x.y")
        .expect_err("the second document's type error must fail the run");
    assert!(matches!(error.failure(), PipelineFailure::TypeMismatch { .. }));
    // The first document (no edits) was already published; the second failed
    // before publishing anything.
    assert_eq!(sink.bytes, b"{\"a\":1}");
    assert_eq!(sink.items, 1);
}

#[test]
fn mismatched_output_format_declines() {
    let registration = jqf_codec_json::registration().expect("json registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid");
    let other = FormatId::try_new("json2").expect("synthetic format id is valid");
    let mut resources = resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled = try_compile_program(".", policy, CompileOptions::new(), &resources).expect("program compiles");
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.json",
        b"42",
        0,
    );
    let mut sink = CollectingSink::new();
    let request = Request::new(&compiled, Input::Whole(b"42"))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(format(), dialect())
        .with_output_format(other, dialect())
        .with_policy(compact_policy())
        .with_framing(FacadeFraming::item_suffix(b""))
        .with_resources(&mut resources)
        .editing();
    let run = match jqf_sdk::execute(request, &mut sink).expect("a declined lane is not an error") {
        Outcome::Served(Report::Sequence(report)) => EditRun::Completed(report),
        Outcome::Served(other) => panic!("unexpected drive report: {other:?}"),
        Outcome::Declined => EditRun::Declined,
    };
    assert!(matches!(run, EditRun::Declined));
    assert!(sink.bytes.is_empty());
}

// ---------------------------------------------------------------------------
// The edit lane: `--edit` widened to TOML and YAML. The SDK lane now serves a
// SINGLE-DOCUMENT format through a one-shot drive (seam 2: one decode, one
// diff, one patch set — no consumed-offset forward-progress requirement) and
// requires FORMAT equality rather than dialect equality (seam 5: TOML's
// `toml-1.0` input and `toml.jqf-1.0@1` output namespaces differ by design).
// Leaf replacements render through the codec's bare-value grammar (seam 3),
// so an edited scalar's span is patched and untouched bytes — comments, key
// order, spelling — survive verbatim.

/// TOML edit policy: single-document (no adjacent values) with compact JSON
/// encode options inherited from the JSON helper's shape.
fn toml_policy() -> PipelinePolicy<'static> {
    // The decode dialect names the request's own input dialect: the provider
    // factories dispatch on `policy.decode.dialect`, so it must match the
    // `with_format` pair the caller passes.
    let dialect: &'static DialectId = Box::leak(Box::new(
        DialectId::try_new(jqf_codec_toml::TOML_1_0_DIALECT_ID).expect("dialect"),
    ));
    PipelinePolicy {
        decode: DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect,
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

fn run_toml_edit(
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
    let compiled =
        try_compile_program(program_source, policy, CompileOptions::new(), &resources).expect("program compiles");
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

#[test]
fn toml_leaf_edit_patches_one_span_and_keeps_comments() {
    // The gate: edit `.port` in a commented TOML file. The diff must
    // produce ONE hunk (the integer span) and leave the comment, the key
    // spelling, and the untouched `name` line byte-identical.
    let mut sink = CollectingSink::new();
    let run = run_toml_edit(b"# server\nport = 8080\nname = \"old\"\n", &mut sink, ".port = 9090")
        .expect("a single-output toml edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(sink.bytes, b"# server\nport = 9090\nname = \"old\"\n");
    assert_eq!(sink.items, 1);
}

#[test]
fn toml_string_leaf_edit_replaces_the_quoted_span() {
    // A CHANGED TOML basic string replaces the whole quoted span with the
    // bare-value rendering; the number sibling survives verbatim.
    let mut sink = CollectingSink::new();
    let run = run_toml_edit(b"port = 8080\nname = \"old\"\n", &mut sink, ".name = \"new\"")
        .expect("a single-output toml edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(sink.bytes, b"port = 8080\nname = \"new\"\n");
    assert_eq!(sink.items, 1);
}

#[test]
fn toml_literal_string_leaf_edit_patches_through_single_quotes() {
    // A TOML literal string `'...'` spans inner content like a basic one; the
    // quote detection recognizes `'` so the patch replaces the whole quoted
    // span. The edited string keeps the LITERAL pair when the new text is
    // literal-safe (142 C1/D3).
    let mut sink = CollectingSink::new();
    let run =
        run_toml_edit(b"name = 'old'\n", &mut sink, ".name = \"new\"").expect("a single-output toml edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(sink.bytes, b"name = 'new'\n");
    assert_eq!(sink.items, 1);
}

#[test]
fn toml_non_assignment_program_echoes_the_document() {
    // A program that reads but does not assign publishes the unchanged bytes
    // verbatim through the one-shot drive.
    let mut sink = CollectingSink::new();
    let run = run_toml_edit(b"# c\nport = 8080\n", &mut sink, ".port").expect("a non-assignment toml edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(sink.bytes, b"# c\nport = 8080\n");
    assert_eq!(sink.items, 1);
}

#[test]
fn toml_insertion_splices_and_keeps_comments() {
    // A structural change (a new key) is a SPLICE, not a whole-document
    // re-encode: the codec's `render_edit_append` places the new statement
    // after the root's last direct statement in TOML local syntax, and the
    // comment and the untouched bytes survive verbatim.
    let mut sink = CollectingSink::new();
    let run =
        run_toml_edit(b"# c\nport = 8080\n", &mut sink, ".host = \"x\"").expect("a structural toml edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(sink.bytes, b"# c\nport = 8080\nhost = \"x\"\n");
    assert_eq!(sink.items, 1);
}

#[test]
fn toml_inline_table_structural_edit_stays_inline() {
    // The splice policy: a member added to an INLINE table
    // renders inside its closing `}` — the table never converts to a
    // `[section]` and the comment survives.
    let mut sink = CollectingSink::new();
    let run = run_toml_edit(b"# cfg\npoint = { x = 1, y = 2 }\n", &mut sink, ".point.z = 3")
        .expect("a structural inline-table toml edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(sink.bytes, b"# cfg\npoint = { x = 1, y = 2, z = 3 }\n");
    assert_eq!(sink.items, 1);
}

#[test]
fn toml_array_of_tables_append_keeps_blocks() {
    // The splice policy: an appended array item on an
    // array-of-tables writes a new `[[p]]` block after the last element,
    // never an inline `p = [...]` re-encode.
    let mut sink = CollectingSink::new();
    let run = run_toml_edit(
        b"[[p]]\nname = \"a\"\n[[p]]\nname = \"b\"\n",
        &mut sink,
        ".p += [{\"name\":\"c\"}]",
    )
    .expect("an array-of-tables append completes");
    assert!(matches!(run, EditRun::Completed(_)));
    assert_eq!(
        sink.bytes,
        b"[[p]]\nname = \"a\"\n[[p]]\nname = \"b\"\n[[p]]\nname = \"c\"\n"
    );
    assert_eq!(sink.items, 1);
}

fn commented_policy(dialect_id: &'static str) -> PipelinePolicy<'static> {
    let dialect: &'static DialectId = Box::leak(Box::new(DialectId::try_new(dialect_id).expect("dialect")));
    PipelinePolicy {
        decode: DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect,
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

fn run_jsonc_edit(
    input: &[u8],
    sink: &mut CollectingSink,
    program_source: &str,
) -> Result<EditRun, PipelineError<String>> {
    let registration = jqf_codec_json::jsonc::registration().expect("jsonc registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::jsonc::FORMAT_ID).expect("format id is valid");
    let input_dialect = || DialectId::try_new(jqf_codec_json::jsonc::TRAILING_DIALECT_ID).expect("input dialect");
    let output_dialect = || DialectId::try_new(jqf_codec_json::jsonc::JQF_1_0_DIALECT_ID).expect("output dialect");
    let mut resources = resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled =
        try_compile_program(program_source, policy, CompileOptions::new(), &resources).expect("program compiles");
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.jsonc",
        input,
        0,
    );
    let request = Request::new(&compiled, Input::Whole(input))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(format(), input_dialect())
        .with_output_format(format(), output_dialect())
        .with_policy(commented_policy(jqf_codec_json::jsonc::TRAILING_DIALECT_ID))
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

fn run_json5_edit(
    input: &[u8],
    sink: &mut CollectingSink,
    program_source: &str,
) -> Result<EditRun, PipelineError<String>> {
    let registration = jqf_codec_json::json5::registration().expect("json5 registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::json5::FORMAT_ID).expect("format id is valid");
    let input_dialect = || DialectId::try_new(jqf_codec_json::json5::DOCUMENT_DIALECT_ID).expect("input dialect");
    let output_dialect = || DialectId::try_new(jqf_codec_json::json5::JQF_1_0_DIALECT_ID).expect("output dialect");
    let mut resources = resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled =
        try_compile_program(program_source, policy, CompileOptions::new(), &resources).expect("program compiles");
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.json5",
        input,
        0,
    );
    let request = Request::new(&compiled, Input::Whole(input))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(format(), input_dialect())
        .with_output_format(format(), output_dialect())
        .with_policy(commented_policy(jqf_codec_json::json5::DOCUMENT_DIALECT_ID))
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

#[test]
fn jsonc_comment_write_inserts_a_slash_comment_before_the_member() {
    let mut sink = CollectingSink::new();
    let run =
        run_jsonc_edit(b"{\"a\":1}", &mut sink, ".a.@comment = [\"note\"]").expect("a jsonc comment write completes");
    assert!(matches!(run, EditRun::Completed(_)));
    let text = String::from_utf8_lossy(&sink.bytes);
    assert!(text.contains("// note"), "jsonc comment write emits //: {text}");
    assert!(text.contains("\"a\""), "member survives: {text}");
    assert_eq!(sink.items, 1);
}

#[test]
fn json5_comment_write_inserts_a_slash_comment_before_the_member() {
    let mut sink = CollectingSink::new();
    let run =
        run_json5_edit(b"{\"a\":1}", &mut sink, ".a.@comment = [\"note\"]").expect("a json5 comment write completes");
    assert!(matches!(run, EditRun::Completed(_)));
    let text = String::from_utf8_lossy(&sink.bytes);
    assert!(text.contains("// note"), "json5 comment write emits //: {text}");
    assert!(text.contains("\"a\""), "member survives: {text}");
    assert_eq!(sink.items, 1);
}

/// The JSONC removal seam (comment-aware cuts): each case pins what leaves
/// with the removed member and what stays. These are the only tests in the
/// repo that drive `jsonc_render_edit_remove`.
#[test]
fn jsonc_removal_cuts_are_comment_aware() {
    let cases: &[(&[u8], &str, &str)] = &[
        // A leading comment block leaves WITH its member.
        (
            b"{\n  \"a\": 1,\n  // lead\n  \"b\": 2,\n  \"c\": 3\n}\n",
            "del(.b)",
            "{\n  \"a\": 1,\n  \"c\": 3\n}\n",
        ),
        // Removing the FIRST member cuts the FOLLOWING comma.
        (b"{\n  \"a\": 1,\n  \"b\": 2\n}\n", "del(.a)", "{\n  \"b\": 2\n}\n"),
        // Removing the LAST member cuts the PRECEDING comma.
        (b"{\n  \"a\": 1,\n  \"b\": 2\n}\n", "del(.b)", "{\n  \"a\": 1\n}\n"),
        // The lone member leaves an empty container: its owned whitespace
        // includes the wrap before the closer, which would otherwise strand
        // `{\n}`.
        (b"{\n  \"a\": 1\n}\n", "del(.a)", "{}\n"),
        // Array items cut with one adjacent comma, layout preserved.
        (b"[\n  1,\n  2,\n  3\n]\n", "del(.[1])", "[\n  1,\n  3\n]\n"),
    ];
    for (input, program, expected) in cases {
        let mut sink = CollectingSink::new();
        let run = run_jsonc_edit(input, &mut sink, program).expect("a single-output edit completes");
        assert!(matches!(run, EditRun::Completed(_)), "{program} on {input:?}");
        assert_eq!(
            String::from_utf8_lossy(&sink.bytes),
            *expected,
            "{program} on {input:?}"
        );
    }
}

/// A block-comment seam (`/* */` on its own line) is one the backward walk
/// cannot name, so the whole removal declines to the floor: the surviving
/// members' attached comment facts re-emit as `//` lines.
#[test]
fn jsonc_removal_at_a_block_comment_seam_falls_to_the_floor() {
    let mut sink = CollectingSink::new();
    let run = run_jsonc_edit(b"{\n  /* keep */\n  \"a\": 1,\n  \"b\": 2\n}\n", &mut sink, "del(.b)")
        .expect("a single-output edit completes");
    assert!(matches!(run, EditRun::Completed(_)));
    let text = String::from_utf8_lossy(&sink.bytes);
    assert!(
        text.contains("\"a\"") && !text.contains("\"b\""),
        "member removed, survivor kept: {text}"
    );
}

/// The JSON5 removal seam names single-quoted strings: a member whose key or
/// value (or an array item) is spelled `'...'` cuts whole, leaving no stray
/// quote byte. Each case's cut is byte-for-byte the shape its double-quoted
/// twin takes (the one-line container keeps the whitespace run after its
/// following comma — the shared `member_cut` law), where before the widening
/// every single-quoted row declined to the whole-document floor or cut short
/// of the closing quote.
#[test]
fn json5_removal_cuts_single_quoted_members_whole() {
    let cases: &[(&[u8], &str, &str)] = &[
        // A single-quoted key AND value leave together, through both quotes.
        (b"{'a': 'x', b: 2}\n", "del(.a)", "{ b: 2}\n"),
        // Double-quoted key, single-quoted value: the cut covers the value's
        // own quote pair.
        (b"{\"a\": 'x', \"b\": 2}\n", "del(.a)", "{ \"b\": 2}\n"),
        // An array item cut with one adjacent comma, its quotes included.
        (b"[1, 'x', 3]\n", "del(.[1])", "[1, 3]\n"),
        // An escaped quote inside the removed body stays inside the cut.
        (b"[1, 'it\\'s', 3]\n", "del(.[1])", "[1, 3]\n"),
        // The double-quoted twins keep their pinned answers.
        (b"{\"a\": \"x\", \"b\": 2}\n", "del(.a)", "{ \"b\": 2}\n"),
        (b"[\"x\", \"y\"]\n", "del(.[0])", "[ \"y\"]\n"),
    ];
    for (input, program, expected) in cases {
        let mut sink = CollectingSink::new();
        let run = run_json5_edit(input, &mut sink, program).expect("a single-output edit completes");
        assert!(matches!(run, EditRun::Completed(_)), "{program} on {input:?}");
        assert_eq!(
            String::from_utf8_lossy(&sink.bytes),
            *expected,
            "{program} on {input:?}"
        );
    }
}
