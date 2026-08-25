//! The whole-document route's LAZY default.
//!
//! The engine lowers a whole-document consumption to a container-span frontier
//! of 1 by default: the codec's validating scan still visits every byte (the
//! corrupt-late law is untouched — only container NODE construction defers),
//! and a subtree nothing touches costs its span record and nothing else.
//!
//! These tests drive the REAL choke point — `CompiledProgram::try_requirement`
//! under the default policy — through the JSON provider, so they fail before
//! the flip (whole-document consumers got frontier 0, eager) and pass after.
//! `decode_eager` forces frontier 0 through the policy override, which is the
//! same override the force-lazy differential's env var flows through and must
//! keep winning.
//!
//! Two assertions are re-based from the plan's sketch onto the mechanism that
//! actually landed: touching a deferred subtree materializes a FRESH owned
//! value (nothing is written back onto the document), so `node_count` does not
//! grow on touch — the deferral win is that the document is SMALLER than an
//! eager build, and that untouched siblings stay spans.

mod common;

use jqf_codec_core::{CodecRunContext, DecodeRequest, DiagnosticPolicy, ValidationMode};
use jqf_data::DialectId;
use jqf_data::{Document, NodeId, Value};
use jqf_engine::{CodecInputOutcome, CodecInputResult, CodecRequirementPolicy, EngineResult, try_compile_program};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

const CREDITS: u32 = 4_096;

/// The deep fixture test 1 navigates: `.a` and `.b` are the depth-1 containers
/// a frontier of 1 defers; `.a.deep` is what the touch materializes.
const DEEP: &[u8] = br#"{"a":{"deep":[1,2,3]},"b":{"deep":[4,5,6]}}"#;

/// Representative documents for the identity projection test: nested and
/// flat containers, empty containers, scalar roots, integers and decimals of
/// every spelling class, escapes and unicode, and duplicate keys.
const FIXTURES: &[&[u8]] = &[
    DEEP,
    br#"[[],{},[[[]]],{"k":{"j":[]}}]"#,
    br#"{"e":"caf\u00e9\n","n":[1e3,-0.5,12345678901234567890]}"#,
    br#"[{"dup":1,"dup":2}]"#,
    br#"{"only":"scalar"}"#,
    b"[]",
    b"7",
    br#"{"big":123456789012345678901234567890,"neg":-0.000000000000000000000001}"#,
    br#"{"unicode":"\u00e9\u4e2d\u6587\ud83d\ude00","esc":"\"\\/\b\f\n\r\t"}"#,
    br#"[true,false,null,"str",-1.5e10,0,0.0]"#,
    br#"{"a":[{"b":1},{"b":2},{"c":3}]}"#,
];

fn source(bytes: &[u8]) -> ResolvedSource<'_> {
    ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "lazy-test.json",
        bytes,
        0,
    )
}

/// Decodes `bytes` as one whole document under `identity`, with the frontier
/// override given (or the engine default when `None`), through the JSON
/// provider's whole-document route. Returns the published document.
fn decode(bytes: &[u8], frontier_override: Option<u32>) -> Document<'_> {
    let mut resources = common::resources();
    let policy = match frontier_override {
        Some(depth) => {
            CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly).with_lazy_frontier(depth)
        }
        None => CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly),
    };
    let program = try_compile_program("keys", policy, &resources).expect("keys compiles");
    let mut provider = jqf_codec_json::registration()
        .expect("registration")
        .decoder()
        .expect("decoder")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new("rfc8259").expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: jqf_codec_json::VALUE_SEPARATORS,
            },
            &mut resources,
        )
        .expect("provider");
    // The requirement travels WITH the program: `try_requirement` is the
    // engine's choke point under test, not a hand-built frontier.
    let requirement = program.try_requirement(&resources).expect("requirement");
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources).expect("open");
    let mut run = CodecRunContext::new(&mut resources);
    run.set_cooperative_credits(CREDITS);
    let result = session.decode(&mut run).expect("decode");
    let (outcome, _report) = CodecInputResult::try_from_access(result).expect("handoff").into_parts();
    let CodecInputOutcome::Result(EngineResult::Located(located)) = outcome else {
        panic!("expected a located whole-document outcome");
    };
    located.product().document().try_clone().expect("document clone")
}

/// Decodes with the engine's DEFAULT frontier decision (lazy by default).
fn decode_lazy(bytes: &[u8]) -> Document<'_> {
    decode(bytes, None)
}

/// Decodes with the explicit eager override (frontier 0).
fn decode_eager(bytes: &[u8]) -> Document<'_> {
    decode(bytes, Some(0))
}

/// The node id of one member of an OBJECT node by key.
fn member(document: &Document<'_>, node: NodeId, key: &str) -> NodeId {
    let handle = document.node_handle(node).expect("node handle");
    let view = document.value_view(handle).expect("value view");
    let object = view.object().expect("object projection").expect("object node");
    object.get(key).expect("member exists").node()
}

/// Whether a node is an unmaterialized container span.
fn is_deferred(document: &Document<'_>, node: NodeId) -> bool {
    let handle = document.node_handle(node).expect("node handle");
    document
        .value_view(handle)
        .expect("value view")
        .is_container_span()
        .expect("span check")
}

/// The lazy-vs-eager identity oracle: both sides are independent decodes of
/// the same bytes, rendered through the derive `Debug` (which prints the
/// structure recursively, never an allocation identity) — the crate's own
/// established comparison for a deferred span against its eager value.
fn values_semantically_equal(left: &Value, right: &Value) -> bool {
    format!("{left:?}") == format!("{right:?}")
}

#[test]
fn a_deep_document_builds_only_the_nodes_that_are_touched() {
    // IDENTITY (`.`) is a whole-document consumer, so it
    // decodes EAGER (the skeleton-drop law — the lazy skeleton would only be
    // re-parsed on materialization and retained beside the materialized
    // tree). The LAZY default belongs to the programs that provably do not
    // materialize the whole document — `keys` (the decode function's
    // program) reads only the root's member identities, so it stays on the
    // lazy frontier and defers the deep containers.
    let document = decode_lazy(DEEP);
    let eager = decode_eager(DEEP);

    // The lazy-default program's decode defers: the whole-document route
    // engages the lazy frontier and builds strictly fewer nodes than the
    // eager build of the same bytes.
    assert!(
        document.container_span_count() > 0,
        "the lazy-default whole-document route must defer by default"
    );
    assert!(
        document.node_count() < eager.node_count(),
        "deferral must build fewer nodes (lazy {} vs eager {})",
        document.node_count(),
        eager.node_count(),
    );

    // Both depth-1 containers are spans before anything is touched.
    let b = member(&document, document.root(), "b");
    assert!(is_deferred(&document, b), ".b must start deferred");

    // Touch `.a.deep`: materialize `.a` (a fresh owned value — nothing is
    // written back onto the document) and read its `.deep` array.
    let a = member(&document, document.root(), "a");
    let before = document.node_count();
    let mut touch_resources = common::resources();
    let a_value = document
        .materialize_node(document.node_handle(a).expect("a handle"), &mut touch_resources)
        .expect("touching .a materializes it");
    let Value::Object(a_object) = a_value else {
        panic!("a must materialize to an object");
    };
    let Value::Array(deep) = a_object.get("deep").expect("a.deep exists") else {
        panic!("a.deep must materialize to an array");
    };
    assert_eq!(deep.len(), 3, "a.deep carries its three elements");

    // The materialization is FRESH (a documented deviation from the plan's
    // sketch, which assumed a write-back builder): the document's node count
    // is unchanged and the untouched sibling is still a span.
    assert_eq!(
        document.node_count(),
        before,
        "materialization is fresh; the document does not grow"
    );
    assert!(
        is_deferred(&document, b),
        ".b was never touched and must still be an unbuilt span"
    );
}

#[test]
fn a_lazy_document_projects_identically_to_an_eager_one() {
    for fixture in FIXTURES {
        let mut lazy_resources = common::resources();
        let mut eager_resources = common::resources();
        let lazy = decode_lazy(fixture)
            .materialize_root(&mut lazy_resources)
            .expect("lazy materialize");
        let eager = decode_eager(fixture)
            .materialize_root(&mut eager_resources)
            .expect("eager materialize");
        assert!(
            values_semantically_equal(&lazy, &eager),
            "fixture: {:?}",
            core::str::from_utf8(fixture).expect("fixtures are UTF-8"),
        );
    }
}

/// The override survives: an explicit frontier (the force-lazy differential's
/// knob) still wins over the default and defeats the deferral.
#[test]
fn an_explicit_eager_override_defeats_the_lazy_default() {
    let document = decode_eager(DEEP);
    assert_eq!(
        document.container_span_count(),
        0,
        "frontier 0 must build every container eagerly"
    );
}
