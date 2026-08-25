//! Located route: a single child stays a Located element; a plural member is a stream (`RequirementMismatch`), not a
//! packed array. Tree-level range location of the same plural spelling is `locate.rs`.

use jqf_codec_core::{
    AccessFootprint, AccessGuarantees, AccessOutcome, AccessRequirement, CodecDemand, CodecError, CodecFailureKind,
    CodecRunContext, DecodeRequest, DemandClause, DiagnosticPolicy, ExactPath, ExactSelectionRecord, PortableStep,
    ValidationMode,
};
use jqf_data::{DialectId, Value};
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
        "test.html",
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
    let registration = jqf_codec_html::registration().expect("registration");
    let dialect = DialectId::try_new(jqf_codec_html::HTML_DOCUMENT_DIALECT_ID).expect("dialect");
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

fn member(name: &str) -> PortableStep {
    PortableStep::SemanticMember(name.to_owned())
}

fn body_p() -> [PortableStep; 2] {
    [member("body"), member("p")]
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

/// Two `p` children: the located session declines so the floor can stream. The tree locator returns `Located::Range`
/// for this spelling — see `locate::tests::a_plural_child_member_locates_a_range`.
#[test]
fn a_plural_member_declines_located_so_the_floor_can_stream() {
    let mut resources = resources();
    let error = decode_located(b"<body><p>1</p><p>2</p></body>", &body_p(), &mut resources)
        .expect_err("plural member is not one Located document");
    assert_eq!(error.kind(), CodecFailureKind::RequirementMismatch);
}

/// One `p` child under `body` stays a Located element whose value is `["hi"]`.
#[test]
fn a_single_child_member_stays_located_element() {
    let mut resources = resources();
    let result = decode_located(b"<body><p>hi</p></body>", &body_p(), &mut resources).expect("located");
    assert_eq!(render(&materialize_located(&result, &mut resources)), r#"["hi"]"#);
}

/// `body[0]` on two paragraphs locates the first paragraph.
#[test]
fn an_index_step_stays_located() {
    let mut resources = resources();
    let result = decode_located(
        b"<body><p>1</p><p>2</p></body>",
        &[member("body"), PortableStep::SemanticIndex(0)],
        &mut resources,
    )
    .expect("located");
    assert_eq!(render(&materialize_located(&result, &mut resources)), r#"["1"]"#);
}

/// A member that matches no child is a `TypeMismatch` observation, not a hard error.
#[test]
fn a_missing_member_stays_a_type_mismatch() {
    let mut resources = resources();
    let result = decode_located(
        b"<body><p>1</p></body>",
        &[member("body"), member("nope")],
        &mut resources,
    )
    .expect("observation");
    let AccessOutcome::Located(located) = result.outcome() else {
        panic!("expected Located");
    };
    assert!(
        matches!(located.result(), ExactSelectionRecord::TypeMismatch { .. }),
        "missing member must mismatch, got {:?}",
        located.result()
    );
}
