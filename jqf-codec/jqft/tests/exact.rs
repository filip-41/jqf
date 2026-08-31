//! Native Exact/Located access for jqft and jqfjson text.
//!
//! Pins Direct-bind product shape (adapter None, slot 1, selection republished as root), last-wins identity with Whole
//! navigate, range decline kind, attribute fallback, and adjacent `---` `consumed_offset`.

use jqf_codec_core::{
    AccessAdapter, AccessFootprint, AccessGuarantees, AccessOutcome, AccessRequirement, AccessResult, CodecDemand,
    CodecFailureKind, CodecRunContext, DecodeRequest, DemandClause, DiagnosticPolicy, ExactPath, ExactSelectionRecord,
    ValidationMode,
};
use jqf_data::{CountDemand, CountRow, CountVerdict, DialectId, ExpandedName, Value, ValueKind};
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

fn source<'a>(bytes: &'a [u8], label: &'a str) -> ResolvedSource<'a> {
    ResolvedSource::new(SourceRef::new(SourceId::new(1), SourceKind::Input), label, bytes, 0)
}

fn demand(resources: &ResourceContext<'_>) -> CodecDemand {
    let mut demand = CodecDemand::try_new(resources);
    demand.try_insert(&DemandClause::SemanticRoot).expect("semantic root");
    demand.try_insert(&DemandClause::ValueShape).expect("value shape");
    demand
}

#[derive(Clone, Copy, Debug)]
enum Step<'a> {
    Member(&'a str),
    Index(i64),
    Range,
}

fn exact_requirement(resources: &ResourceContext<'_>, path: &[Step<'_>]) -> AccessRequirement {
    let mut exact = ExactPath::try_new(resources);
    for step in path {
        match *step {
            Step::Member(member) => exact.try_push_semantic_member(member, resources).expect("member"),
            Step::Index(index) => exact.try_push_semantic_index(index, resources),
            Step::Range => exact.try_push_semantic_range(Some(0), Some(1), resources),
        }
    }
    let footprint = AccessFootprint::try_exact(exact, resources);
    AccessRequirement::try_exact(
        footprint,
        demand(resources),
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        resources,
    )
    .expect("exact requirement")
}

#[derive(Clone, Copy)]
struct Format {
    registration: fn() -> Result<jqf_codec_core::CodecRegistration<'static>, jqf_codec_core::RegistrationError>,
    dialect: &'static str,
    label: &'static str,
}

const JQFT: Format = Format {
    registration: jqf_codec_jqft::registration_jqft,
    dialect: jqf_codec_jqft::JQFT_DOCUMENT_DIALECT_ID,
    label: "test.jqft",
};

const JQFJSON: Format = Format {
    registration: jqf_codec_jqft::registration_jqfjson,
    dialect: jqf_codec_jqft::JQFJSON_DOCUMENT_DIALECT_ID,
    label: "test.jqfjson",
};

fn decode<'s>(
    bytes: &'s [u8],
    format: Format,
    path: Option<&[Step<'_>]>,
    allow_adjacent: bool,
    resources: &mut ResourceContext<'_>,
) -> Result<AccessResult<'s>, jqf_codec_core::CodecError> {
    let registration = (format.registration)().expect("registration");
    let mut provider = registration.decoder().expect("decoder").create_provider(
        source(bytes, format.label),
        DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: &DialectId::try_new(format.dialect).expect("dialect"),
            options: None,
            allow_adjacent_values: allow_adjacent,
            value_separator: &[],
        },
        resources,
    )?;
    let requirement = match path {
        Some(path) => exact_requirement(resources, path),
        None => AccessRequirement::try_whole(
            demand(resources),
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            resources,
        )
        .expect("whole"),
    };
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, resources)?;
    let mut run = CodecRunContext::new(resources);
    run.set_cooperative_credits(4_096);
    session.decode(&mut run)
}

fn decode_count<'s>(
    bytes: &'s [u8],
    format: Format,
    path: &[Step<'_>],
    resources: &mut ResourceContext<'_>,
) -> Result<AccessResult<'s>, jqf_codec_core::CodecError> {
    let registration = (format.registration)().expect("registration");
    let mut provider = registration.decoder().expect("decoder").create_provider(
        source(bytes, format.label),
        DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: &DialectId::try_new(format.dialect).expect("dialect"),
            options: None,
            allow_adjacent_values: false,
            value_separator: &[],
        },
        resources,
    )?;
    let requirement = exact_requirement(resources, path).with_count(CountDemand {
        row: CountRow::Container,
        path: Vec::new(),
        range: None,
        probe: Vec::new(),
        filter: None,
    });
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, resources)?;
    let mut run = CodecRunContext::new(resources);
    run.set_cooperative_credits(4_096);
    session.decode(&mut run)
}

fn located<'a, 's>(result: &'a AccessResult<'s>) -> &'a jqf_codec_core::LocatedOutcome<'s> {
    let AccessOutcome::Located(located) = result.outcome() else {
        panic!("expected Located, got {:?}", result.outcome());
    };
    located
}

fn materialize_root(result: &AccessResult<'_>, resources: &mut ResourceContext<'_>) -> Value {
    let document = match result.outcome() {
        AccessOutcome::FullDocument(product) => product.document(),
        AccessOutcome::Located(located) => located.product().document(),
    };
    document.materialize_root(resources).expect("materialize")
}

fn walk_value(value: &Value, path: &[Step<'_>]) -> Value {
    let mut current = value.clone();
    for step in path {
        current = unwrap_tags(current);
        current = match (*step, current) {
            (Step::Member(name), Value::Object(object)) => object.get(name).expect("member").clone(),
            (Step::Index(index), Value::Array(array)) => {
                let len = i64::try_from(array.len()).expect("len");
                let position = if index < 0 { len.checked_add(index) } else { Some(index) }
                    .and_then(|p| usize::try_from(p).ok())
                    .expect("index");
                array.get(position).expect("item").clone()
            }
            _ => panic!("cannot walk {step:?}"),
        };
    }
    current
}

fn unwrap_tags(mut value: Value) -> Value {
    while let Value::Tagged { payload, .. } = value {
        value = (*payload).clone();
    }
    value
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(true) => "true".into(),
        Value::Bool(false) => "false".into(),
        Value::Number(number) => format!("{number:?}"),
        Value::String(text) => format!("s{}", text.as_str()),
        Value::Bytes(bytes) => format!("h{:?}", bytes.as_slice()),
        Value::Array(array) => {
            let items: Vec<String> = array.iter().map(render).collect();
            format!("[{}]", items.join(","))
        }
        Value::Object(object) => {
            let entries: Vec<String> = object
                .iter()
                .map(|entry| format!("{}:{}", entry.key(), render(entry.value())))
                .collect();
            format!("{{{}}}", entries.join(","))
        }
        Value::Tagged { tag, payload } => format!("@{} {}", tag.as_str(), render(payload)),
        Value::LocalDate(_) | Value::LocalTime(_) | Value::LocalDateTime(_) | Value::OffsetDateTime(_) => {
            format!("{value:?}")
        }
    }
}

fn jqft(body: &str) -> Vec<u8> {
    let mut bytes = b"%jqft 1\n".to_vec();
    bytes.extend_from_slice(body.as_bytes());
    bytes
}

/// Direct Exact binds slot 1, adapter None, and republishes the selection as the product root — jqft and jqfjson.
#[test]
fn text_exact_binds_slot_1_adapter_none_node_is_product_root() {
    for (bytes, format, route) in [
        (jqft("{a: 1}\n"), JQFT, jqf_codec_jqft::JQFT_LOCATED_PHYSICAL_ROUTE_ID),
        (
            b"{\"a\":1}\n".to_vec(),
            JQFJSON,
            jqf_codec_jqft::JQFJSON_LOCATED_PHYSICAL_ROUTE_ID,
        ),
    ] {
        let mut resources = resources();
        let result = decode(&bytes, format, Some(&[Step::Member("a")]), false, &mut resources).expect("exact");
        let selected = located(&result);
        let ExactSelectionRecord::Node { node, .. } = selected.result() else {
            panic!("node");
        };
        assert_eq!(*node, selected.product().document().root_handle());
        assert_eq!(result.report().adapter(), AccessAdapter::None);
        assert_eq!(result.report().route().expect("receipt").route(), route);
        assert_eq!(result.report().route().expect("receipt").slot().get(), 1);
        assert_eq!(
            render(&materialize_root(&result, &mut resources)),
            render(&walk_value(
                &{
                    let whole = decode(&bytes, format, None, false, &mut resources).expect("whole");
                    materialize_root(&whole, &mut resources)
                },
                &[Step::Member("a")],
            ))
        );
    }
}

/// Exact + count publishes the locate span. The validate walk already proved the
/// range and counted its children; count must not re-parse the hit.
#[test]
fn exact_count_demand_uses_the_locate_span() {
    let cases: &[(&[u8], Format)] = &[
        (b"%jqft 1\n{users: [1, 2, 3]}\n", JQFT),
        (b"{\"users\":[1,2,3]}", JQFJSON),
    ];
    for (bytes, format) in cases {
        let mut resources = resources();
        let result = decode_count(bytes, *format, &[Step::Member("users")], &mut resources).expect("count demand");
        let selected = located(&result);
        let ExactSelectionRecord::Node { node, .. } = selected.result() else {
            panic!("node")
        };
        let document = selected.product().document();
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
                .count_children_from(
                    *node,
                    &CountDemand {
                        row: CountRow::Container,
                        path: Vec::new(),
                        range: None,
                        probe: Vec::new(),
                        filter: None,
                    },
                    &mut resources
                )
                .expect("span count"),
            CountVerdict::Count(3)
        );
    }
}

#[test]
fn exact_count_demand_last_wins_replaces_the_pointer() {
    let cases: &[(&[u8], Format)] = &[
        (b"%jqft 1\n{users: [1], users: [1, 2, 3]}\n", JQFT),
        (b"{\"users\":[1],\"users\":[1,2,3]}", JQFJSON),
    ];
    for (bytes, format) in cases {
        let mut resources = resources();
        let result = decode_count(bytes, *format, &[Step::Member("users")], &mut resources).expect("last-wins");
        let selected = located(&result);
        let ExactSelectionRecord::Node { node, .. } = selected.result() else {
            panic!("node")
        };
        let document = selected.product().document();
        assert_eq!(
            document.container_span_child_count(*node).expect("handle"),
            Some(3),
            "last-wins replaces the cached count with the winner"
        );
        assert_eq!(
            document
                .count_children_from(
                    *node,
                    &CountDemand {
                        row: CountRow::Container,
                        path: Vec::new(),
                        range: None,
                        probe: Vec::new(),
                        filter: None,
                    },
                    &mut resources
                )
                .expect("winning span count"),
            CountVerdict::Count(3)
        );
    }
}

#[test]
fn exact_count_demand_still_validates_unread_bytes() {
    let cases: &[(&[u8], Format)] = &[
        (b"%jqft 1\n{users: [1, 2, 3]} false\n", JQFT),
        (b"{\"users\":[1,2,3]} false", JQFJSON),
    ];
    for (bytes, format) in cases {
        let mut resources = resources();
        let result = decode_count(bytes, *format, &[Step::Member("users")], &mut resources);
        assert!(
            result.is_err(),
            "unread trailing value must still fail Exact validation over {}",
            String::from_utf8_lossy(bytes)
        );
    }
}

/// Last-wins duplicate keys and nested member+index match Whole navigate (decode both, compare values).
#[test]
fn last_wins_and_nested_member_index_match_whole_navigate() {
    let cases: &[(&[u8], &[Step<'_>])] = &[
        (b"%jqft 1\n{a: 1, a: 2}\n", &[Step::Member("a")]),
        (
            b"%jqft 1\n{a: {b: 1}, a: {b: 2}}\n",
            &[Step::Member("a"), Step::Member("b")],
        ),
        (
            b"%jqft 1\n{catalog: [{name: 1}, {name: 2}]}\n",
            &[Step::Member("catalog"), Step::Index(1), Step::Member("name")],
        ),
        (
            b"%jqft 1\n{items: [10, 20]}\n",
            &[Step::Member("items"), Step::Index(-1)],
        ),
        (
            b"%jqft 1\n{items: [10, 20,]}\n",
            &[Step::Member("items"), Step::Index(-1)],
        ),
        (b"%jqft 1\n{a: 1,}\n", &[Step::Member("a")]),
        (b"{\"a\":1,\"a\":2}\n", &[Step::Member("a")]),
        (
            b"{\"a\":{\"b\":1},\"a\":{\"b\":2}}\n",
            &[Step::Member("a"), Step::Member("b")],
        ),
    ];
    for (bytes, path) in cases {
        let format = if bytes.starts_with(b"%jqft") { JQFT } else { JQFJSON };
        let mut resources = resources();
        let exact = decode(bytes, format, Some(path), false, &mut resources).expect("exact");
        let whole = decode(bytes, format, None, false, &mut resources).expect("whole");
        let exact_value = render(&materialize_root(&exact, &mut resources));
        let whole_value = render(&walk_value(&materialize_root(&whole, &mut resources), path));
        assert_eq!(
            exact_value,
            whole_value,
            "path {path:?} over {}",
            String::from_utf8_lossy(bytes)
        );
    }
}

/// Duplicate-key last-wins still scans later keys: a corrupt unread sibling fails Exact as Whole.
#[test]
fn last_wins_still_scans_later_keys() {
    let bytes = b"%jqft 1\n{a: 1, a: 2, pad: [}\n";
    let mut resources = resources();
    assert!(decode(bytes, JQFT, Some(&[Step::Member("a")]), false, &mut resources).is_err());
    assert!(decode(bytes, JQFT, None, false, &mut resources).is_err());
}

/// Range on text Exact declines `RequirementMismatch`, not `InternalContract`.
#[test]
fn range_declines_requirement_mismatch() {
    for (bytes, format) in [(jqft("[1, 2]\n"), JQFT), (b"[1,2]\n".to_vec(), JQFJSON)] {
        let mut resources = resources();
        let error = decode(&bytes, format, Some(&[Step::Range]), false, &mut resources)
            .expect_err("range declines the located route");
        assert_eq!(error.kind(), CodecFailureKind::RequirementMismatch);
    }
}

/// A range step still validates unread bytes before declining, so a corrupt sibling fails like Whole.
#[test]
fn range_on_corrupt_input_fails_like_whole() {
    let bytes = b"%jqft 1\n[1, [}\n";
    let mut resources = resources();
    assert!(decode(bytes, JQFT, Some(&[Step::Range]), false, &mut resources).is_err());
    assert!(decode(bytes, JQFT, None, false, &mut resources).is_err());
}

/// Member/Index miss: `Missing` or `TypeMismatch`, byte-identical to Whole navigate (the record, not a raise).
#[test]
fn member_index_miss_matches_whole_record() {
    let mut resources = resources();
    let bytes = jqft("{a: 1}\n");
    let missing = decode(&bytes, JQFT, Some(&[Step::Member("missing")]), false, &mut resources).expect("missing");
    assert!(matches!(
        located(&missing).result(),
        ExactSelectionRecord::Missing { step_index: 0, .. }
    ));
    let mismatch = decode(
        &bytes,
        JQFT,
        Some(&[Step::Member("a"), Step::Member("b")]),
        false,
        &mut resources,
    )
    .expect("mismatch");
    assert!(matches!(
        located(&mismatch).result(),
        ExactSelectionRecord::TypeMismatch {
            step_index: 1,
            actual_type: ValueKind::Number,
            ..
        }
    ));
}

/// Attribute demand does not Direct-bind the text Exact slot as absence; bind succeeds via `CompleteDocumentExact`.
#[test]
fn attribute_demand_does_not_direct_bind_text_exact() {
    for (bytes, format) in [(jqft("{a: 1}\n"), JQFT), (b"{\"a\":1}\n".to_vec(), JQFJSON)] {
        let mut resources = resources();
        let registration = (format.registration)().expect("registration");
        let mut provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(
                source(&bytes, format.label),
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(format.dialect).expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: &[],
                },
                &mut resources,
            )
            .expect("provider");
        assert!(
            !provider.supports_attribute_absence(),
            "markup .& is real; absence stays false"
        );
        let mut path = ExactPath::try_new(&resources);
        path.try_push_semantic_member("a", &resources).expect("member");
        let footprint = AccessFootprint::try_exact(path, &resources);
        let mut demand = CodecDemand::try_new(&resources);
        demand
            .try_insert(&DemandClause::Attribute(
                ExpandedName::try_new("", "x").expect("expanded name"),
            ))
            .expect("attribute");
        let requirement = AccessRequirement::try_exact(
            footprint,
            demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement");
        let handle = provider.bind(&requirement).expect("bind must not HardMismatch");
        assert_eq!(handle.slot().get(), 0);
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let mut context = CodecRunContext::new(&mut resources);
        context.set_cooperative_credits(4_096);
        let result = session.decode(&mut context).expect("decode");
        assert_eq!(result.report().adapter(), AccessAdapter::CompleteDocumentExact);
    }
}

/// Adjacent `---` : first document Exact; `consumed_offset` lets the next document start (slot 1 is not recycled).
#[test]
fn adjacent_stream_consumed_offset_starts_the_next_document() {
    let bytes = b"%jqft 1\n{a: 1}\n---\n{a: 2}\n";
    let mut resources = resources();
    let registration = jqf_codec_jqft::registration_jqft().expect("registration");
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source(bytes, "stream.jqft"),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_jqft::JQFT_DOCUMENT_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: true,
                value_separator: &[],
            },
            &mut resources,
        )
        .expect("provider");
    let requirement = exact_requirement(&resources, &[Step::Member("a")]);
    let handle = provider.bind(&requirement).expect("bind");
    let first = {
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run).expect("first")
    };
    assert_eq!(first.report().adapter(), AccessAdapter::None);
    let offset = first.report().consumed_offset().expect("consumed_offset");
    let first_value = render(&materialize_root(&first, &mut resources));
    let mut session = provider.open_at(&handle, offset, &mut resources).expect("open_at");
    let mut run = CodecRunContext::new(&mut resources);
    run.set_cooperative_credits(4_096);
    let second = session.decode(&mut run).expect("second");
    let second_value = render(&materialize_root(&second, &mut resources));
    assert_ne!(first_value, second_value, "second document must be a new Exact session");
}

/// Tags are payload-transparent on Exact the way Whole navigate is: a wrapper does not consume a step, and a tagged
/// member value is still the located product.
#[test]
fn tagged_object_member_matches_whole() {
    for (body, path) in [
        (r#"@tag("t") {a: 1}"#, [Step::Member("a")].as_slice()),
        (r#"{a: @tag("t") 1}"#, [Step::Member("a")].as_slice()),
    ] {
        let bytes = jqft(&format!("{body}\n"));
        let mut resources = resources();
        let exact = decode(&bytes, JQFT, Some(path), false, &mut resources).expect("exact");
        let whole = decode(&bytes, JQFT, None, false, &mut resources).expect("whole");
        assert_eq!(
            render(&materialize_root(&exact, &mut resources)),
            render(&walk_value(&materialize_root(&whole, &mut resources), path)),
            "{body}"
        );
    }
}
