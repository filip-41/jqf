//! The Exact/Located scoped route equals whole-decode-then-navigate: a
//! plural member is a stream, not a packed array.

use jqf_codec_core::{
    AccessFootprint, AccessGuarantees, AccessOutcome, AccessRequirement, CodecDemand, CodecError, CodecFailureKind,
    CodecRunContext, DecodeRequest, DemandClause, DiagnosticPolicy, ExactPath, ExactSelectionRecord, PortableStep,
    ValidationMode,
};
use jqf_data::{CountDemand, CountRow, CountVerdict, DialectId, Value};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

static CONTROL: ContinueControl = ContinueControl;

fn resources() -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(4_096).expect("work"),
    )
    .expect("resources")
}

fn source(bytes: &[u8]) -> ResolvedSource<'_> {
    ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.xml",
        bytes,
        0,
    )
}

fn demand(resources: &ResourceContext<'_>) -> CodecDemand {
    let mut demand = CodecDemand::try_new(resources);
    demand.try_insert(&DemandClause::SemanticRoot).expect("semantic root");
    demand.try_insert(&DemandClause::ValueShape).expect("value shape");
    demand
}

fn located_requirement(resources: &ResourceContext<'_>, steps: &[PortableStep]) -> AccessRequirement {
    let mut path = ExactPath::try_new(resources);
    for step in steps {
        match step {
            PortableStep::SemanticMember(name) => {
                path.try_push_semantic_member(name, resources).expect("member");
            }
            PortableStep::SemanticIndex(index) => {
                path.try_push_semantic_index(*index, resources);
            }
            PortableStep::SemanticRange { start, end } => {
                path.try_push_semantic_range(*start, *end, resources);
            }
        }
    }
    let footprint = AccessFootprint::try_exact(path, resources);
    AccessRequirement::try_exact(
        footprint,
        demand(resources),
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        resources,
    )
    .expect("requirement")
}

fn decode_located<'s>(
    bytes: &'s [u8],
    steps: &[PortableStep],
    resources: &mut ResourceContext<'_>,
) -> Result<jqf_codec_core::AccessResult<'s>, CodecError> {
    let registration = jqf_codec_xml::registration().expect("registration");
    let dialect = DialectId::try_new(jqf_codec_xml::XML_DOCUMENT_DIALECT_ID).expect("dialect");
    let mut provider = registration.decoder().expect("decoder").create_provider(
        source(bytes),
        DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: &dialect,
            options: None,
            allow_adjacent_values: false,
            value_separator: &[],
        },
        resources,
    )?;
    let requirement = located_requirement(resources, steps);
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, resources)?;
    let mut run = CodecRunContext::new(resources);
    run.set_cooperative_credits(4_096);
    session.decode(&mut run)
}

fn container_count() -> CountDemand {
    CountDemand {
        row: CountRow::Container,
        path: Vec::new(),
        range: None,
        probe: Vec::new(),
        filter: None,
    }
}

fn decode_located_count<'s>(
    bytes: &'s [u8],
    steps: &[PortableStep],
    resources: &mut ResourceContext<'_>,
) -> Result<jqf_codec_core::AccessResult<'s>, CodecError> {
    let registration = jqf_codec_xml::registration().expect("registration");
    let dialect = DialectId::try_new(jqf_codec_xml::XML_DOCUMENT_DIALECT_ID).expect("dialect");
    let mut provider = registration.decoder().expect("decoder").create_provider(
        source(bytes),
        DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: &dialect,
            options: None,
            allow_adjacent_values: false,
            value_separator: &[],
        },
        resources,
    )?;
    let requirement = located_requirement(resources, steps).with_count(container_count());
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, resources)?;
    let mut run = CodecRunContext::new(resources);
    run.set_cooperative_credits(4_096);
    session.decode(&mut run)
}

fn member(name: &str) -> PortableStep {
    PortableStep::SemanticMember(name.to_owned())
}

fn materialize_located(result: &jqf_codec_core::AccessResult<'_>, resources: &mut ResourceContext<'_>) -> Value {
    let AccessOutcome::Located(located) = result.outcome() else {
        panic!("expected Located, got {:?}", result.outcome());
    };
    let ExactSelectionRecord::Node { node, .. } = located.result() else {
        panic!("expected a located node, got {:?}", located.result());
    };
    located
        .product()
        .document()
        .materialize_node_with(&mut jqf_data::MaterializeWorkspace::new(), *node, resources)
        .expect("materialize")
}

fn render(value: &Value) -> String {
    match value {
        Value::String(text) => format!("{text:?}"),
        Value::Array(array) => {
            let items: Vec<String> = array.iter().map(render).collect();
            format!("[{}]", items.join(", "))
        }
        other => format!("{other:?}"),
    }
}

#[test]
fn a_plural_member_declines_located_so_the_floor_can_stream() {
    let mut resources = resources();
    let bytes = b"<r><b>1</b><b>2</b></r>";
    let error =
        decode_located(bytes, &[member("b")], &mut resources).expect_err("plural member is not one Located document");
    assert_eq!(error.kind(), CodecFailureKind::RequirementMismatch);
    // Whole-decode-then-navigate yields the two `b` children as a stream
    // (`["1"]` then `["2"]`), never the packed array `[["1"], ["2"]]`.
    let root = jqf_codec_xml::decode_document(source(bytes), &mut resources).expect("whole");
    let Value::Array(children) = root else {
        panic!("document element is an array of children, got {root:?}");
    };
    assert_eq!(children.len(), 2, "two matching children");
    assert_eq!(render(children.get(0).expect("first")), r#"["1"]"#);
    assert_eq!(render(children.get(1).expect("second")), r#"["2"]"#);
}

/// Nested unique members Exact equals Whole then navigate. Siblings prove
/// the path selected by name, not by being the only child.
#[test]
fn a_nested_unique_member_equals_whole_then_navigate() {
    let mut resources = resources();
    let bytes = b"<doc><pad>p</pad><nested><deep>x</deep><skip>y</skip></nested></doc>";
    let result = decode_located(bytes, &[member("nested"), member("deep")], &mut resources).expect("located");
    let exact = materialize_located(&result, &mut resources);
    let root = jqf_codec_xml::decode_document(source(bytes), &mut resources).expect("whole");
    let Value::Array(doc_children) = &root else {
        panic!("document element is an array of children, got {root:?}");
    };
    assert_eq!(doc_children.len(), 2, "pad and nested");
    let Value::Array(nested_children) = doc_children.get(1).expect("nested") else {
        panic!("nested is an array of children");
    };
    let deep = nested_children.get(0).expect("deep");
    assert_eq!(render(&exact), render(deep));
    assert_eq!(render(&exact), r#"["x"]"#);
}

#[test]
fn a_nested_plural_member_declines_located() {
    let mut resources = resources();
    let bytes = b"<doc><g><b>1</b><b>2</b></g></doc>";
    let error = decode_located(bytes, &[member("g"), member("b")], &mut resources)
        .expect_err("nested plural member is not one Located document");
    assert_eq!(error.kind(), CodecFailureKind::RequirementMismatch);
}

#[test]
fn a_single_child_member_stays_located_element() {
    let mut resources = resources();
    let result = decode_located(b"<r><b>1</b></r>", &[member("b")], &mut resources).expect("located");
    assert_eq!(render(&materialize_located(&result, &mut resources)), r#"["1"]"#);
}

#[test]
fn an_index_step_stays_located() {
    let mut resources = resources();
    let result = decode_located(
        b"<r><b>1</b><b>2</b></r>",
        &[PortableStep::SemanticIndex(0)],
        &mut resources,
    )
    .expect("located");
    assert_eq!(render(&materialize_located(&result, &mut resources)), r#"["1"]"#);
}

#[test]
fn a_missing_member_stays_a_type_mismatch() {
    let mut resources = resources();
    let result = decode_located(b"<r><b>1</b></r>", &[member("x")], &mut resources).expect("observation");
    let AccessOutcome::Located(located) = result.outcome() else {
        panic!("expected Located");
    };
    assert!(
        matches!(located.result(), ExactSelectionRecord::TypeMismatch { .. }),
        "missing member must mismatch, got {:?}",
        located.result()
    );
}

#[test]
fn a_single_child_index_after_member_stays_located() {
    let mut resources = resources();
    let result = decode_located(
        b"<r><b>1</b></r>",
        &[member("b"), PortableStep::SemanticIndex(0)],
        &mut resources,
    )
    .expect("located");
    assert_eq!(render(&materialize_located(&result, &mut resources)), r#""1""#);
}

/// Exact + count publishes the locate element's span. The proving parse already
/// counted direct children; count must not re-parse the hit.
#[test]
fn exact_count_demand_uses_the_locate_span() {
    let mut resources = resources();
    let result = decode_located_count(
        b"<root><users><a/><b/><c/></users></root>",
        &[member("users")],
        &mut resources,
    )
    .expect("count demand");
    let AccessOutcome::Located(located) = result.outcome() else {
        panic!("located")
    };
    let ExactSelectionRecord::Node { node, .. } = located.result() else {
        panic!("node")
    };
    let document = located.product().document();
    assert!(
        document
            .value_view(*node)
            .expect("view")
            .is_container_span()
            .expect("span"),
        "count Exact must publish the locate span, not a rematerialized tree"
    );
    assert_eq!(document.node_count(), 1, "skeleton Exact is one container-span node");
    assert_eq!(
        document.container_span_child_count(*node).expect("handle"),
        Some(3),
        "Exact count records cardinality on the proving walk"
    );
    assert_eq!(
        document
            .count_children_from(*node, &container_count(), &mut resources)
            .expect("span count"),
        CountVerdict::Count(3)
    );
}

#[test]
fn exact_count_demand_empty_element_records_zero() {
    let mut resources = resources();
    let result = decode_located_count(b"<root><users/></root>", &[member("users")], &mut resources).expect("empty");
    let AccessOutcome::Located(located) = result.outcome() else {
        panic!("located")
    };
    let ExactSelectionRecord::Node { node, .. } = located.result() else {
        panic!("node")
    };
    let document = located.product().document();
    assert_eq!(document.container_span_child_count(*node).expect("handle"), Some(0));
    assert_eq!(
        document
            .count_children_from(*node, &container_count(), &mut resources)
            .expect("empty count"),
        CountVerdict::Count(0)
    );
}

#[test]
fn exact_count_demand_nested_member_uses_the_locate_span() {
    let mut resources = resources();
    let result = decode_located_count(
        b"<doc><nested><deep><a/><b/></deep></nested></doc>",
        &[member("nested"), member("deep")],
        &mut resources,
    )
    .expect("nested count");
    let AccessOutcome::Located(located) = result.outcome() else {
        panic!("located")
    };
    let ExactSelectionRecord::Node { node, .. } = located.result() else {
        panic!("node")
    };
    let document = located.product().document();
    assert!(
        document
            .value_view(*node)
            .expect("view")
            .is_container_span()
            .expect("span"),
        "nested Exact count still publishes the final hit as a span"
    );
    assert_eq!(
        document.node_count(),
        1,
        "nested Exact count is one container-span node"
    );
    assert_eq!(document.container_span_child_count(*node).expect("handle"), Some(2));
    assert_eq!(
        document
            .count_children_from(*node, &container_count(), &mut resources)
            .expect("nested span count"),
        CountVerdict::Count(2)
    );
}

#[test]
fn exact_count_demand_still_validates_unread_bytes() {
    let mut resources = resources();
    decode_located_count(
        b"<root><users><a/><b/><c/></users></root> trailing",
        &[member("users")],
        &mut resources,
    )
    .expect_err("unread trailing bytes must still fail Exact validation");
}
