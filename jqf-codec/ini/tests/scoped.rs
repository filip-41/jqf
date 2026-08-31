//! Native Exact/Located route for the three flat-config grammars.
//!
//! Direct-bind slot 1 republishes the hit as the product root. Last-wins keys
//! match Whole navigate; a duplicate INI section is terminal on Exact as on
//! Whole; a range step declines so the binder's floor serves it; a corrupt
//! unread byte fails Exact as Whole.

use jqf_codec_core::{
    AccessAdapter, AccessFootprint, AccessGuarantees, AccessOutcome, AccessRequirement, AccessResult, CodecDemand,
    CodecError, CodecFailureKind, CodecRunContext, DecodeRequest, DemandClause, DiagnosticPolicy, ExactPath,
    ExactSelectionRecord, PortableStep, ValidationMode,
};
use jqf_data::{DialectId, Value, ValueKind};
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
        "test.ini",
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

fn whole_requirement(resources: &ResourceContext<'_>) -> AccessRequirement {
    AccessRequirement::try_whole(
        demand(resources),
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        resources,
    )
    .expect("requirement")
}

#[derive(Clone, Copy)]
enum Format {
    Properties,
    Ini,
    Dotenv,
}

fn member(name: &str) -> PortableStep {
    PortableStep::SemanticMember(name.to_owned())
}

fn decode<'s>(
    format: Format,
    bytes: &'s [u8],
    requirement: &AccessRequirement,
    resources: &mut ResourceContext<'_>,
) -> Result<AccessResult<'s>, CodecError> {
    let (registration, dialect) = match format {
        Format::Properties => (
            jqf_codec_ini::registration().expect("registration"),
            jqf_codec_ini::PROPERTIES_JDK_DIALECT_ID,
        ),
        Format::Ini => (
            jqf_codec_ini::registration_ini().expect("registration"),
            jqf_codec_ini::INI_JQF_STRICT_DIALECT_ID,
        ),
        Format::Dotenv => (
            jqf_codec_ini::registration_dotenv().expect("registration"),
            jqf_codec_ini::DOTENV_JQF_STRICT_DIALECT_ID,
        ),
    };
    let dialect = DialectId::try_new(dialect).expect("dialect");
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
    let handle = provider.bind(requirement).expect("bind");
    let mut session = provider.open(&handle, resources)?;
    let mut run = CodecRunContext::new(resources);
    run.set_cooperative_credits(4_096);
    session.decode(&mut run)
}

fn decode_located<'s>(
    format: Format,
    bytes: &'s [u8],
    steps: &[PortableStep],
    resources: &mut ResourceContext<'_>,
) -> Result<AccessResult<'s>, CodecError> {
    let requirement = located_requirement(resources, steps);
    decode(format, bytes, &requirement, resources)
}

fn decode_whole<'s>(
    format: Format,
    bytes: &'s [u8],
    resources: &mut ResourceContext<'_>,
) -> Result<AccessResult<'s>, CodecError> {
    let requirement = whole_requirement(resources);
    decode(format, bytes, &requirement, resources)
}

fn located_node_value(result: &AccessResult<'_>, resources: &mut ResourceContext<'_>) -> Value {
    let AccessOutcome::Located(located) = result.outcome() else {
        panic!("expected Located, got {:?}", result.outcome());
    };
    let ExactSelectionRecord::Node { node, .. } = located.result() else {
        panic!("expected a located node, got {:?}", located.result());
    };
    assert_eq!(*node, located.product().document().root_handle());
    located
        .product()
        .document()
        .materialize_node(*node, resources)
        .expect("materialize")
}

fn whole_navigate(result: &AccessResult<'_>, keys: &[&str], resources: &mut ResourceContext<'_>) -> Value {
    let AccessOutcome::FullDocument(product) = result.outcome() else {
        panic!("expected FullDocument");
    };
    let mut value = product.document().materialize_root(resources).expect("root");
    for key in keys {
        let Value::Object(object) = value else {
            panic!("expected object while navigating {keys:?}");
        };
        value = object.get(key).expect("member").clone();
    }
    value
}

/// Direct Exact binds slot 1 with adapter None; the located node is the product root.
#[test]
fn ini_exact_binds_slot_1_adapter_none() {
    let mut resources = resources();
    let result = decode_located(Format::Ini, b"k=1\n", &[member("k")], &mut resources).expect("located");
    assert_eq!(result.report().adapter(), AccessAdapter::None);
    assert_eq!(result.report().route().expect("receipt").slot().get(), 1);
    let AccessOutcome::Located(located) = result.outcome() else {
        panic!("expected Located");
    };
    let ExactSelectionRecord::Node { node, .. } = located.result() else {
        panic!("expected Node");
    };
    assert_eq!(*node, located.product().document().root_handle());
    let Value::String(text) = located_node_value(&result, &mut resources) else {
        panic!("expected string");
    };
    assert_eq!(text.as_str(), "1");
}

/// Last `k=` wins on Exact, matching Whole navigate of the same key.
#[test]
fn ini_exact_last_wins_matches_floor() {
    let bytes = b"k=1\nk=2\n";
    for format in [Format::Properties, Format::Dotenv] {
        let mut resources = resources();
        let exact = decode_located(format, bytes, &[member("k")], &mut resources).expect("exact");
        let whole = decode_whole(format, bytes, &mut resources).expect("whole");
        let exact_value = located_node_value(&exact, &mut resources);
        let floor = whole_navigate(&whole, &["k"], &mut resources);
        let Value::String(text) = &exact_value else {
            panic!("expected string");
        };
        let Value::String(floor_text) = &floor else {
            panic!("expected string");
        };
        assert_eq!(text.as_str(), floor_text.as_str());
        assert_eq!(text.as_str(), "2");
    }

    let bytes = b"[a]\nk=1\nk=2\n";
    let mut resources = resources();
    let exact = decode_located(Format::Ini, bytes, &[member("a"), member("k")], &mut resources).expect("ini last-wins");
    let whole = decode_whole(Format::Ini, bytes, &mut resources).expect("whole");
    let exact_value = located_node_value(&exact, &mut resources);
    let floor = whole_navigate(&whole, &["a", "k"], &mut resources);
    let Value::String(text) = &exact_value else {
        panic!("expected string");
    };
    let Value::String(floor_text) = &floor else {
        panic!("expected string");
    };
    assert_eq!(text.as_str(), floor_text.as_str());
    assert_eq!(text.as_str(), "2");
}

/// Duplicate section is terminal on Exact as on Whole.
#[test]
fn ini_exact_duplicate_section_fails_like_whole() {
    let bytes = b"[a]\nx=1\n[a]\n";
    let mut resources = resources();
    let whole = decode_whole(Format::Ini, bytes, &mut resources).expect_err("whole duplicate");
    let exact =
        decode_located(Format::Ini, bytes, &[member("a"), member("x")], &mut resources).expect_err("exact duplicate");
    assert_eq!(whole.kind(), exact.kind());
    assert_eq!(whole.kind(), CodecFailureKind::InvalidInput);
}

/// A range step declines so the binder's whole-document floor serves it.
#[test]
fn ini_exact_range_declines_requirement_mismatch() {
    let mut resources = resources();
    let error = decode_located(
        Format::Ini,
        b"k=1\n",
        &[PortableStep::SemanticRange {
            start: Some(0),
            end: Some(1),
        }],
        &mut resources,
    )
    .expect_err("range declines");
    assert_eq!(error.kind(), CodecFailureKind::RequirementMismatch);
}

/// `.section.key` Exact product matches Whole then navigate; unused siblings are not in the product.
#[test]
fn ini_exact_section_key_matches_whole_navigate() {
    let bytes = b"[db]\nhost = localhost\n[pad]\nx=1\n";
    let mut resources = resources();
    let exact = decode_located(Format::Ini, bytes, &[member("db"), member("host")], &mut resources).expect("exact");
    let whole = decode_whole(Format::Ini, bytes, &mut resources).expect("whole");
    let exact_value = located_node_value(&exact, &mut resources);
    let floor = whole_navigate(&whole, &["db", "host"], &mut resources);
    let Value::String(text) = &exact_value else {
        panic!("expected string");
    };
    let Value::String(floor_text) = &floor else {
        panic!("expected string");
    };
    assert_eq!(text.as_str(), floor_text.as_str());
    assert_eq!(text.as_str(), "localhost");

    let section = decode_located(Format::Ini, bytes, &[member("db")], &mut resources).expect("section");
    let section_value = located_node_value(&section, &mut resources);
    let Value::Object(object) = &section_value else {
        panic!("expected section object");
    };
    assert_eq!(object.len(), 1);
    assert!(object.get("host").is_some());
    assert!(object.get("pad").is_none());
    assert!(object.get("x").is_none());
}

/// Index against an object is `TypeMismatch` plus a null product; a missing key is `Missing` plus null.
#[test]
fn ini_exact_index_and_missing_are_negative_observations() {
    let mut resources = resources();
    let mismatch = decode_located(
        Format::Properties,
        b"k=1\n",
        &[PortableStep::SemanticIndex(0)],
        &mut resources,
    )
    .expect("type mismatch");
    let AccessOutcome::Located(located) = mismatch.outcome() else {
        panic!("expected Located");
    };
    assert!(
        matches!(
            located.result(),
            ExactSelectionRecord::TypeMismatch {
                actual_type: ValueKind::Object,
                ..
            }
        ),
        "got {:?}",
        located.result()
    );
    let Value::Null = located
        .product()
        .document()
        .materialize_root(&mut resources)
        .expect("null")
    else {
        panic!("expected null product");
    };

    let nested = decode_located(
        Format::Properties,
        b"k=1\n",
        &[member("k"), member("x")],
        &mut resources,
    )
    .expect("member on string");
    let AccessOutcome::Located(located) = nested.outcome() else {
        panic!("expected Located");
    };
    assert!(
        matches!(
            located.result(),
            ExactSelectionRecord::TypeMismatch {
                actual_type: ValueKind::String,
                step_index: 1,
                ..
            }
        ),
        "got {:?}",
        located.result()
    );

    let missing = decode_located(Format::Properties, b"k=1\n", &[member("nope")], &mut resources).expect("missing");
    let AccessOutcome::Located(located) = missing.outcome() else {
        panic!("expected Located");
    };
    assert!(
        matches!(located.result(), ExactSelectionRecord::Missing { step_index: 0, .. }),
        "got {:?}",
        located.result()
    );
}

/// A corrupt unread byte fails Exact as Whole (validate-everything-first).
#[test]
fn ini_exact_corrupt_unread_fails_like_whole() {
    let bytes = b"k=1\n\xff";
    let mut resources = resources();
    let whole = decode_whole(Format::Properties, bytes, &mut resources).expect_err("whole corrupt");
    let exact = decode_located(Format::Properties, bytes, &[member("k")], &mut resources).expect_err("exact corrupt");
    assert_eq!(whole.kind(), exact.kind());
    assert_eq!(whole.kind(), CodecFailureKind::InvalidInput);
}
