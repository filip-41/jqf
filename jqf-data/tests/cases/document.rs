use core::ops::ControlFlow;

use jqf_data::{
    AccountedDocumentBuilder, AccountedIntrinsicTag, AccountedOccurrenceKey, AccountedSemanticNode,
    AuthoritativeEmptyFamilies, BatchLimit, DataError, DiagnosticCoverage, Document, DocumentCapability,
    DocumentCapabilityFamily, DocumentCapacity, FactPayload, LocalOwnerRef, MaterializeWorkspace, ReaderDemand,
    ReaderPoll, TopologyBatch, Value, ValueKind, unbounded_batch_limit,
};
use jqf_resource::{
    ContinueControl, Control, ControlError, ControlOutcome, RequestAccount, ResourceContext, ResourceLimits,
    WorkAdmission, WorkMeter,
};

fn bool_document(value: bool) -> Document<'static> {
    let resources = Box::leak(Box::new(unlimited_resources()));
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
    let root = builder
        .add_node("test.bool", AccountedSemanticNode::Bool(value), None, resources)
        .expect("root is added");
    builder.finish(root, resources).expect("document finishes")
}

fn unlimited_resources() -> ResourceContext<'static> {
    let control = Box::leak(Box::new(ContinueControl));
    let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
    let account = RequestAccount::try_new(limits).expect("account allocates");
    let work = WorkMeter::try_new_v1(4096).expect("maximum work slice is valid");
    ResourceContext::new(account, control, work).expect("context starts")
}

/// A control whose cancellation trips on demand.
struct FlipControl(core::cell::Cell<bool>);

impl Control for FlipControl {
    fn check(&self) -> ControlOutcome {
        if self.0.get() {
            ControlOutcome::Cancelled
        } else {
            ControlOutcome::Continue
        }
    }
}

/// A context whose work slice carries exactly `budget` units.
fn slice_resources(budget: u32) -> ResourceContext<'static> {
    let control = Box::leak(Box::new(ContinueControl));
    let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
    let account = RequestAccount::try_new(limits).expect("account allocates");
    let work = WorkMeter::try_new_v1(budget).expect("work budget is valid");
    ResourceContext::new(account, control, work).expect("context starts")
}

fn cancellable_resources() -> (ResourceContext<'static>, &'static FlipControl) {
    let control = Box::leak(Box::new(FlipControl(core::cell::Cell::new(false))));
    let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
    let account = RequestAccount::try_new(limits).expect("account allocates");
    let work = WorkMeter::try_new_v1(4096).expect("maximum work slice is valid");
    let context = ResourceContext::new(account, control, work).expect("context starts");
    (context, control)
}

/// A boolean document carrying `fact_count` attached null facts on its root.
fn document_with_facts(fact_count: usize) -> Document<'static> {
    let resources = Box::leak(Box::new(unlimited_resources()));
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
    let root = builder
        .add_node("test.bool", AccountedSemanticNode::Bool(true), None, resources)
        .expect("root is added");
    let payload = FactPayload::Null;
    for _ in 0..fact_count {
        builder
            .add_fact(
                LocalOwnerRef::Node(root),
                "test.note",
                "test.null",
                1,
                &payload,
                resources,
            )
            .expect("fact is added");
    }
    builder.finish(root, resources).expect("document finishes")
}

/// A builder that demanded nothing in particular publishes a document whose coverage carries every DATA capability and
/// none of the source ones: the default is complete for what the builder itself produced, and silent about what only a
/// source-bearing decode can claim. Coverage survives finalization unchanged, so a caller reads the same authority
/// before and after.
#[test]
fn complete_builders_publish_data_coverage() {
    let document = bool_document(true);
    let coverage = document.coverage();
    for capability in [
        DocumentCapability::SemanticRoot,
        DocumentCapability::SemanticNodes,
        DocumentCapability::IntrinsicTags,
        DocumentCapability::Topology,
        DocumentCapability::AttachedFacts,
    ] {
        assert!(coverage.contains(capability), "{capability:?} is complete");
    }
    assert!(!coverage.contains(DocumentCapability::WholeSource));
    assert_eq!(coverage.diagnostic_coverage(), DiagnosticCoverage::NotRequested);

    let mut resources = unlimited_resources();
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("accounted builder starts");
    let root = builder
        .add_node("test.bool", AccountedSemanticNode::Bool(true), None, &resources)
        .expect("accounted root is added");
    let accounted = builder.finish(root, &resources).expect("accounted builder finishes");
    assert_eq!(accounted.coverage(), coverage);

    let mut finalizer_builder =
        AccountedDocumentBuilder::try_new("test", None).expect("accounted finalizer builder starts");
    let finalizer_root = finalizer_builder
        .add_node("test.bool", AccountedSemanticNode::Bool(false), None, &resources)
        .expect("accounted finalizer root is added");
    let mut finalizer = finalizer_builder
        .begin_finish(finalizer_root, &resources)
        .expect("finalizer starts");
    let finalized_document = loop {
        match finalizer.poll(&mut resources).expect("finalizer polls") {
            jqf_data::DocumentFinalizationPoll::Pending => {
                assert!(
                    resources
                        .try_begin_next_cooperative_entry(4096)
                        .expect("next cooperative entry starts")
                );
            }
            jqf_data::DocumentFinalizationPoll::Ready(document) => break document,
        }
    };
    assert_eq!(finalized_document.coverage(), coverage);
}

/// A publisher that KNOWS a family is empty can say so, and the claim reaches the finished document: an absent family
/// and a family authoritatively declared empty are different answers, and only the declaration lets a consumer treat
/// "no attributes" as a fact rather than as missing coverage.
#[test]
fn publishers_can_declare_header_only_empty_authority() {
    let resources = unlimited_resources();
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
    builder.set_authoritative_empty_families(AuthoritativeEmptyFamilies::from_family(
        DocumentCapabilityFamily::Attributes,
    ));
    builder.set_diagnostic_coverage(DiagnosticCoverage::AuthoritativeEmpty);
    let root = builder
        .add_node("test.bool", AccountedSemanticNode::Bool(true), None, &resources)
        .expect("root is added");
    let document = builder.finish(root, &resources).expect("document finishes");
    assert!(
        document
            .coverage()
            .authoritative_empty_families()
            .contains(DocumentCapabilityFamily::Attributes)
    );
    assert_eq!(
        document.coverage().diagnostic_coverage(),
        DiagnosticCoverage::AuthoritativeEmpty
    );
}

/// Declaring a family authoritatively empty and then populating it is a contradiction the builder must refuse, in
/// whichever order the two arrive: a document that both claims no attached facts and carries one would let a consumer
/// prove either answer.
#[test]
fn contradictory_empty_family_declarations_are_rejected_deterministically() {
    let attached = AuthoritativeEmptyFamilies::from_family(DocumentCapabilityFamily::AttachedFacts);
    let resources = unlimited_resources();
    let mut fact_builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
    fact_builder.set_authoritative_empty_families(attached);
    let fact_root = fact_builder
        .add_node("test.bool", AccountedSemanticNode::Bool(true), None, &resources)
        .expect("fact root is added");
    let payload = FactPayload::Bool(true);
    fact_builder
        .add_fact(
            LocalOwnerRef::Node(fact_root),
            "test.note",
            "test.bool",
            1,
            &payload,
            &resources,
        )
        .expect("fact is added");
    assert!(matches!(
        fact_builder.finish(fact_root, &resources),
        Err(DataError::ContradictoryCoverage {
            family: DocumentCapabilityFamily::AttachedFacts,
        })
    ));

    // The reverse arrival: the populated family lands BEFORE the empty declaration is made, and finish still refuses.
    let mut reversed_builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
    let reversed_root = reversed_builder
        .add_node("test.bool", AccountedSemanticNode::Bool(true), None, &resources)
        .expect("reversed root is added");
    let payload = FactPayload::Bool(false);
    reversed_builder
        .add_fact(
            LocalOwnerRef::Node(reversed_root),
            "test.note",
            "test.bool",
            1,
            &payload,
            &resources,
        )
        .expect("reversed fact is added");
    reversed_builder.set_authoritative_empty_families(attached);
    assert!(matches!(
        reversed_builder.finish(reversed_root, &resources),
        Err(DataError::ContradictoryCoverage {
            family: DocumentCapabilityFamily::AttachedFacts,
        })
    ));
}

/// The compact fact arena interns kinds and roles, so several facts share one binding slot and one fact's kind may be
/// another's role. The reader must resolve each fact back to the exact identity it was given: repeated bindings must
/// not alias, and a mixed kind/role reuse must not swap the two back out.
#[test]
fn fact_reader_resolves_repeated_and_mixed_compact_bindings_exactly() {
    let mut resources = unlimited_resources();
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
    let root = builder
        .add_node("test.bool", AccountedSemanticNode::Bool(true), None, &resources)
        .expect("root is added");
    for (role, kind) in [
        ("test.note", "test.text"),
        ("test.note", "test.text"),
        ("test.annotation", "test.number"),
    ] {
        let payload = FactPayload::Null;
        builder
            .add_fact(LocalOwnerRef::Node(root), role, kind, 1, &payload, &resources)
            .expect("fact is added");
    }
    let document = builder.finish(root, &resources).expect("document finishes");
    let mut reader = document.fact_reader(&mut resources).expect("fact reader starts");
    let limit = BatchLimit::new(8).expect("valid batch limit");
    let ReaderPoll::Batch(batch) = reader.poll_batch(limit, &mut resources).expect("fact batch reads") else {
        panic!("expected one fact batch");
    };
    let actual = batch
        .iter()
        .map(|fact| (fact.role().as_str(), fact.kind().as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        [
            ("test.note", "test.text"),
            ("test.note", "test.text"),
            ("test.annotation", "test.number"),
        ]
    );
}

/// An object member overwritten before finalization is not the object's semantics, so a CYCLE that existed only through
/// the overwritten occurrence is not a cycle in the published document: materialization must answer with the surviving
/// member instead of refusing a loop the finished object does not contain.
#[test]
fn overwritten_cycle_is_not_part_of_object_semantics() {
    let mut resources = unlimited_resources();
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
    let root = builder
        .add_node(
            "test.object",
            AccountedSemanticNode::Object {
                member_role: "test.member",
            },
            None,
            &resources,
        )
        .expect("object root is added");
    let final_value = builder
        .add_node("test.bool", AccountedSemanticNode::Bool(true), None, &resources)
        .expect("final value is added");
    for target in [root, final_value] {
        builder
            .add_occurrence(
                LocalOwnerRef::Node(root),
                "test.member",
                Some(AccountedOccurrenceKey::Text("item")),
                target,
                &resources,
            )
            .expect("member occurrence is added");
    }
    let document = builder.finish(root, &resources).expect("document finishes");

    let Value::Object(object) = document
        .materialize_root(&mut resources)
        .expect("projection materializes")
    else {
        panic!("root must materialize as an object");
    };
    assert!(matches!(object.get("item"), Some(Value::Bool(true))));
}

/// The same law for the representability refusal: a member overwritten before finalization is gone from the object's
/// semantics, so an unrepresentable node reachable only through the overwritten occurrence must not make the finished
/// document unmaterializable.
#[test]
fn overwritten_unrepresentable_node_is_not_materialized() {
    let mut resources = unlimited_resources();
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
    let root = builder
        .add_node(
            "test.object",
            AccountedSemanticNode::Object {
                member_role: "test.member",
            },
            None,
            &resources,
        )
        .expect("object root is added");
    let discarded = builder
        .add_node("test.raw", AccountedSemanticNode::Unrepresentable, None, &resources)
        .expect("raw node is added");
    let final_value = builder
        .add_node("test.bool", AccountedSemanticNode::Bool(true), None, &resources)
        .expect("final value is added");
    for target in [discarded, final_value] {
        builder
            .add_occurrence(
                LocalOwnerRef::Node(root),
                "test.member",
                Some(AccountedOccurrenceKey::Text("item")),
                target,
                &resources,
            )
            .expect("member occurrence is added");
    }
    let document = builder.finish(root, &resources).expect("document finishes");
    let Value::Object(object) = document
        .materialize_root(&mut resources)
        .expect("projection materializes")
    else {
        panic!("root must materialize as an object");
    };
    assert!(
        matches!(object.get("item"), Some(Value::Bool(true))),
        "the overwriting final value must be the one materialized, not the discarded raw node"
    );
}

/// A materializer is scratch that outlives one failed run. After a cycle refusal it must be left clean — no
/// half-written active set, no retained node marks — so the next materialization over a sound document succeeds
/// instead of inheriting the failure's state.
#[test]
fn reusable_materializer_recovers_after_cycle_failure() {
    let mut resources = unlimited_resources();
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
    let root = builder
        .add_node(
            "test.array",
            AccountedSemanticNode::Array { item_role: "test.item" },
            None,
            &resources,
        )
        .expect("array root is added");
    let independent = builder
        .add_node("test.bool", AccountedSemanticNode::Bool(true), None, &resources)
        .expect("independent node is added");
    builder
        .add_occurrence(LocalOwnerRef::Node(root), "test.item", None, root, &resources)
        .expect("cycle edge is added");
    let document = builder.finish(root, &resources).expect("document finishes");
    let independent = document.node_handle(independent).expect("handle resolves");
    let mut workspace = jqf_data::MaterializeWorkspace::new();
    let root = document.root_handle();

    assert!(matches!(
        document.materialize_node_with(&mut workspace, root, &mut resources),
        Err(DataError::CyclicSemanticGraph)
    ));
    assert!(matches!(
        document.materialize_node_with(&mut workspace, independent, &mut resources),
        Ok(Value::Bool(true))
    ));
}

#[test]
fn try_reserve_and_root_workspace_reuse_are_callable() {
    let mut resources = unlimited_resources();
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
    builder
        .try_reserve(
            DocumentCapacity {
                nodes: 1,
                occurrences: 0,
                stored_text_bytes: 0,
                facts: 0,
            },
            &resources,
        )
        .expect("reserve is a hint");
    let root = builder
        .add_node("test.bool", AccountedSemanticNode::Bool(true), None, &resources)
        .expect("root is added");
    let document = builder.finish(root, &resources).expect("document finishes");
    let mut workspace = MaterializeWorkspace::new();
    assert!(matches!(
        document.materialize_root_with(&mut workspace, &mut resources),
        Ok(Value::Bool(true))
    ));
    assert!(matches!(
        document.materialize_root_with(&mut workspace, &mut resources),
        Ok(Value::Bool(true))
    ));
}

/// An unrepresentable node has no value projection at all, so materialization fails TERMINALLY rather than substituting
/// null or skipping the node: a reader may not project a value for semantics that have none.
#[test]
fn materializing_an_unrepresentable_node_is_terminal() {
    let mut resources = unlimited_resources();
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
    let root = builder
        .add_node("test.raw", AccountedSemanticNode::Unrepresentable, None, &resources)
        .expect("raw root is added");
    let document = builder.finish(root, &resources).expect("document finishes");
    // The representability law lives on the materialization path: an unrepresentable node fails TERMINALLY there, so no
    // reader can project a value for a node whose semantics have none.
    assert!(matches!(
        document.materialize_root(&mut resources),
        Err(DataError::UnrepresentableSemantic)
    ));
}

/// A core intrinsic tag names the category its node already is. Attaching one that disagrees — a string-category core
/// tag on a boolean node — is an invalid document at admission, not a tag the readers must later reconcile against
/// the semantics beneath it.
#[test]
fn core_tags_must_match_the_semantic_category() {
    let resources = unlimited_resources();
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
    assert_eq!(
        builder.add_node(
            "test.bool",
            AccountedSemanticNode::Bool(true),
            Some(AccountedIntrinsicTag::Core {
                tag: "core:string",
                kind: ValueKind::String,
            }),
            &resources,
        ),
        Err(DataError::InvalidDocument)
    );
}

/// A fact map is unique-keyed, and the builder is where that is enforced: admitting two entries under one key would
/// hand every reader a map whose answer depends on which entry it happened to scan first. Work admission's empty-batch
/// guard: when the FIRST grant of a poll is refused, the reader answers Pending rather than handing back an empty batch
/// its consumer would spin on forever, and the refused poll consumes nothing — one replenished slice later the whole
/// table arrives in a single full grant.
#[test]
fn refused_first_grant_pends_and_consumes_nothing() {
    let document = document_with_facts(2);
    let mut resources = slice_resources(1);
    // Drain the single budget unit so the reader's first request is refused.
    assert!(matches!(resources.admit_work_bytes(1), Ok(WorkAdmission::Granted(1))));
    let mut reader = document.fact_reader(&mut resources).expect("fact reader starts");
    let limit = BatchLimit::new(8).expect("valid batch limit");
    assert!(matches!(
        reader.poll_batch(limit, &mut resources),
        Ok(ReaderPoll::Pending)
    ));

    assert!(
        resources
            .try_begin_next_cooperative_entry(4096)
            .expect("next cooperative entry starts")
    );
    let ReaderPoll::Batch(batch) = reader.poll_batch(limit, &mut resources).expect("fact batch reads") else {
        panic!("expected one full batch after replenishment");
    };
    assert_eq!(batch.len(), 2);
}

/// A slice that runs out AFTER items were admitted ends the batch early: the partial count publishes instead of waiting
/// for the whole request, the next poll pends before consuming anything further, and resumption reads exactly the
/// remaining tail before completing.
#[test]
fn exhausted_slice_ends_a_started_batch_partially_then_resumes() {
    let document = document_with_facts(300); // more than one 256-item linear grant
    let mut resources = slice_resources(1);
    let mut reader = document.fact_reader(&mut resources).expect("fact reader starts");
    let limit = BatchLimit::new(512).expect("valid batch limit");

    let ReaderPoll::Batch(first) = reader.poll_batch(limit, &mut resources).expect("first poll") else {
        panic!("expected a partial batch");
    };
    assert_eq!(first.len(), 256);

    assert!(matches!(
        reader.poll_batch(limit, &mut resources),
        Ok(ReaderPoll::Pending)
    ));

    assert!(
        resources
            .try_begin_next_cooperative_entry(4096)
            .expect("next cooperative entry starts")
    );
    let ReaderPoll::Batch(second) = reader.poll_batch(limit, &mut resources).expect("resumed poll") else {
        panic!("expected the resumed tail batch");
    };
    assert_eq!(second.len(), 44);
    assert!(matches!(
        reader.poll_batch(limit, &mut resources),
        Ok(ReaderPoll::End(_))
    ));
}

#[test]
fn facts_drain_visits_every_attached_fact() {
    let document = document_with_facts(2);
    let mut resources = unlimited_resources();
    let mut reader = document.fact_reader(&mut resources).expect("fact reader starts");
    let mut seen = 0;
    let walked = reader
        .drain(&mut resources, |_| {
            seen += 1;
            ControlFlow::<()>::Continue(())
        })
        .expect("facts drain");
    assert_eq!(walked, ControlFlow::Continue(()));
    assert_eq!(seen, 2);
}

#[test]
fn facts_drain_replenishes_a_pending_unbounded_walk() {
    let document = document_with_facts(2);
    let mut resources = slice_resources(1);
    assert!(matches!(resources.admit_work_bytes(1), Ok(WorkAdmission::Granted(1))));
    let mut reader = document.fact_reader(&mut resources).expect("fact reader starts");
    let mut seen = 0;
    let walked = reader
        .drain(&mut resources, |_| {
            seen += 1;
            ControlFlow::<()>::Continue(())
        })
        .expect("pending drain resumes");
    assert_eq!(walked, ControlFlow::Continue(()));
    assert_eq!(seen, 2);
}

#[test]
fn fact_drain_break_stops_before_the_rest() {
    let document = document_with_facts(3);
    let mut resources = unlimited_resources();
    let mut reader = document.fact_reader(&mut resources).expect("fact reader starts");
    let mut seen = 0;
    let stopped = reader
        .drain(&mut resources, |_| {
            seen += 1;
            ControlFlow::Break(seen)
        })
        .expect("drain starts");
    assert_eq!(stopped, ControlFlow::Break(1));
    assert_eq!(seen, 1);
}

#[test]
fn unbounded_batch_limit_is_the_nonzero_max() {
    assert_eq!(unbounded_batch_limit().get(), usize::MAX);
}

/// A control error latches a reader terminal: even once the host stops cancelling, every later poll reports
/// `ReaderFailed` instead of resuming, so a caller can never mistake a half-read walk for complete evidence.
#[test]
fn a_control_failure_latches_the_fact_reader_terminal() {
    let document = document_with_facts(2);
    let (mut resources, cancel) = cancellable_resources();
    let mut reader = document.fact_reader(&mut resources).expect("fact reader starts");
    cancel.0.set(true);
    let limit = BatchLimit::new(8).expect("valid batch limit");
    assert!(matches!(
        reader.poll_batch(limit, &mut resources),
        Err(DataError::Control(ControlError::Cancelled))
    ));

    cancel.0.set(false);
    assert!(matches!(
        reader.poll_batch(limit, &mut resources),
        Err(DataError::ReaderFailed)
    ));
}

/// One topology-reader walk over a fresh builder: every node batch precedes every occurrence batch, both arrive in
/// authored order with resolved roles and keys, and completion binds Topology demand to the exact document.
#[test]
fn topology_reader_emits_nodes_then_occurrences_in_authored_order() {
    let mut resources = unlimited_resources();
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
    let root = builder
        .add_node(
            "test.object",
            AccountedSemanticNode::Object {
                member_role: "test.member",
            },
            None,
            &resources,
        )
        .expect("object root is added");
    let first = builder
        .add_node("test.bool", AccountedSemanticNode::Bool(false), None, &resources)
        .expect("first item is added");
    let second = builder
        .add_node("test.bool", AccountedSemanticNode::Bool(true), None, &resources)
        .expect("second item is added");
    builder
        .add_occurrence(
            LocalOwnerRef::Node(root),
            "test.member",
            Some(AccountedOccurrenceKey::Text("alpha")),
            first,
            &resources,
        )
        .expect("first occurrence is added");
    builder
        .add_occurrence(
            LocalOwnerRef::Node(root),
            "test.member",
            Some(AccountedOccurrenceKey::Text("beta")),
            second,
            &resources,
        )
        .expect("second occurrence is added");
    let document = builder.finish(root, &resources).expect("document finishes");

    let mut reader = document
        .topology_reader(&mut resources)
        .expect("topology reader starts");
    let limit = BatchLimit::new(2).expect("valid batch limit");
    let mut nodes = Vec::new();
    let mut occurrences = Vec::new();
    loop {
        match reader.poll_batch(limit, &mut resources).expect("topology poll") {
            ReaderPoll::Batch(TopologyBatch::Nodes(batch)) => {
                for view in &batch {
                    nodes.push(view.expect("node view").id());
                }
            }
            ReaderPoll::Batch(TopologyBatch::Occurrences(batch)) => {
                for view in &batch {
                    let view = view.expect("occurrence view");
                    occurrences.push((
                        view.role().as_str().to_owned(),
                        view.position(),
                        view.key_text().map(str::to_owned),
                        view.target(),
                    ));
                }
            }
            ReaderPoll::Pending => {}
            ReaderPoll::End(completion) => {
                assert_eq!(completion.demand(), ReaderDemand::Topology);
                assert_eq!(completion.document(), document.key());
                break;
            }
        }
    }
    assert_eq!(nodes, [root, first, second]);
    assert_eq!(
        occurrences,
        [
            ("test.member".to_owned(), 0, Some("alpha".to_owned()), first),
            ("test.member".to_owned(), 1, Some("beta".to_owned()), second),
        ]
    );
}

#[test]
fn fact_maps_reject_duplicate_keys() {
    let resources = unlimited_resources();
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
    let root = builder
        .add_node("test.bool", AccountedSemanticNode::Bool(true), None, &resources)
        .expect("root is added");
    let payload = FactPayload::Map(vec![
        ("same".into(), FactPayload::Bool(false)),
        ("same".into(), FactPayload::Bool(true)),
    ]);
    assert_eq!(
        builder.add_fact(
            LocalOwnerRef::Node(root),
            "test.note",
            "test.map",
            1,
            &payload,
            &resources,
        ),
        Err(DataError::InvalidDocument)
    );
}
