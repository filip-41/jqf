use super::{
    EngineError, EngineRun, EngineRunError, EngineRunStream, GraphMachine, JoinEntry, RunInput, RunPoll, RunStep,
    StageMachine, StepOutcome, equal_key_run, try_run_program, try_run_with_table,
};

use crate::analysis::{CorrelatedScan, PartialSort, correlated_scans};
use crate::program::{
    BinaryKind, LogicalOp, ObjectMemberNode, Program, ProgramNode, ProgramNodeId, StageStart, StageStep, StepAccess,
    VarSlot,
};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use jqf_builtins::codec_result::{CodecInputOutcome, CodecInputResult, EngineResult};
use jqf_builtins::registry::{BuiltinOverloadId, SemanticRevision};

/// Builds a slice of key steps carrying the given per-component optional
/// flags — a compact witness for the exact-step suppression law.
fn steps(flags: &[bool]) -> Vec<StageStep> {
    flags
        .iter()
        .map(|&optional| StageStep::new(StepAccess::Key(String::from("k")), optional))
        .collect()
}

/// A single-`Stage` program over `steps` — the shape production
/// [`try_run_program`] runs for a pure-path residual.
fn stage_program(steps: Vec<StageStep>) -> Program {
    let nodes = vec![ProgramNode::Stage {
        start: StageStart::Current,
        steps,
    }];
    let root = ProgramNodeId::ROOT;
    let split = crate::analysis::analyze(&nodes, root);
    Program::new(nodes, root, split, 0)
}

fn run_stage<'p, 's>(
    program: &'p Program,
    outcome: CodecInputOutcome<'s>,
) -> Result<EngineRun<'p, 's>, jqf_codec_core::CodecError> {
    try_run_program(program, program.split().prefix_len(), outcome, None)
}

/// The root id of a freshly built arena.
const ROOT: ProgramNodeId = ProgramNodeId::ROOT;

fn key(name: &str, optional: bool) -> StageStep {
    StageStep::new(StepAccess::Key(String::from(name)), optional)
}

/// An owned integer value from its canonical spelling (for the try/catch tests
/// that raise runtime errors over an owned scalar input).
fn int_value(text: &str) -> Value {
    Value::Number(jqf_data::Number::try_json_literal(text).expect("integer literal"))
}

use jqf_codec_core::{
    AccessOutcome, AccessResult, DocumentProduct, ExactSelectionRecord, LocatedOutcome, LocatedProduct, SelectionOrigin,
};
use jqf_data::{
    AccountedDocumentBuilder, AccountedIntrinsicTag, AccountedOccurrenceKey, AccountedSemanticNode, Array, FactPayload,
    LocalOwnerRef, NodeId, Number, ObjectBuilder, ObjectKey, Value, ValueKind,
};
use jqf_resource::policy::MismatchPolicy;
use jqf_resource::{ContinueControl, MemoryCategory, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

use crate::compile::CompileOptions;

static CONTROL: ContinueControl = ContinueControl;

fn resources() -> ResourceContext<'static> {
    resources_with_credits(4_096)
}

/// The INDEX lowering's outer reduce is recognized as a keyed collect.
///
/// This is the engagement witness for the keyed-collect fast path: the
/// shape check must match the graph `lower_indexed_fold` actually builds,
/// or the drive silently stays on the generic Modify machinery. The
/// program is compiled the ordinary way (prelude and optimizer included)
/// and every Reduce node in the arena is checked.
#[test]
fn the_index_lowering_is_recognized_as_a_keyed_collect() {
    let res = resources();
    let compiled = compiled_for_execute("INDEX(.[]; .id) | length", &res);
    let arena = compiled.arena();
    let mut reduces = 0usize;
    let mut recognized = 0usize;
    for node in arena {
        if let ProgramNode::Reduce {
            source: _,
            slot,
            init,
            update,
            keyed_collect,
        } = node
        {
            reduces += 1;
            if keyed_collect.is_some() || crate::compile::keyed_collect_keys(arena, *update, *slot, *init).is_some() {
                recognized += 1;
            }
        }
    }
    let user = compiled_for_execute(
        "def INDEX(s; i): reduce s as $r ({}; .[$r|i|tostring] = $r); INDEX(.[]; .id) | length",
        &res,
    );
    assert!(
        user.arena().iter().any(|node| {
            matches!(
                node,
                ProgramNode::Reduce {
                    source,
                    slot,
                    init,
                    update,
                    keyed_collect,
                } if keyed_collect.is_some()
                    || crate::compile::keyed_collect_keys(user.arena(), *update, *slot, *init)
                        .is_some()
            )
        }),
        "user-def arena:\n{}",
        user.arena()
            .iter()
            .enumerate()
            .map(|(i, node)| alloc::format!("[{i}] {node:?}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        compiled.arena().iter().any(|node| matches!(
            node,
            ProgramNode::Reduce {
                keyed_collect: Some(_),
                ..
            }
        )),
        "INDEX lowering must store the keyed-collect key graph on the Reduce"
    );
    assert!(
        recognized >= 1,
        "no reduce matched the keyed-collect shape (arena has {reduces} reduces):\n{}",
        arena
            .iter()
            .enumerate()
            .map(|(i, node)| alloc::format!("[{i}] {node:?}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The streamed-aggregate recognition table: a `[gen] | keyed` pipe at the
/// root is recognized exactly, and its near-twins decline to the floor.
#[test]
fn the_streamed_aggregate_recognizer_matches_group_and_selection_pipes() {
    use crate::exec::GraphMachine;
    let res = resources();
    let recognizes = |source: &str| {
        let compiled = compiled_for_execute(source, &res);
        GraphMachine::streamed_aggregate(compiled.arena(), compiled.root())
    };
    for source in [
        "[inputs] | group_by(.k)",
        "[inputs] | min_by(.k)",
        "[inputs] | max_by(.k)",
    ] {
        if recognizes(source).is_none() {
            let compiled = compiled_for_execute(source, &res);
            let dump = compiled
                .arena()
                .iter()
                .enumerate()
                .map(|(i, node)| alloc::format!("[{i}] {node:?}"))
                .collect::<alloc::vec::Vec<_>>()
                .join("\n");
            panic!("{source} must be recognized; arena:\n{dump}");
        }
    }
    // Near-twins keep today's path: a binding between collect and call, a
    // comma consumer, sort-family modes, and a non-collect upstream.
    for source in [
        "[inputs] as $r | $r | group_by(.k)",
        "[inputs] | (group_by(.k), length)",
        "[inputs] | sort_by(.k)",
        "[inputs] | unique_by(.k)",
        "[.[]] as $a | $a | min_by(.k)",
        "inputs | group_by(.k)",
    ] {
        assert!(recognizes(source).is_none(), "{source} must not be recognized");
    }
}

fn resources_with_credits(credits: u32) -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(credits).expect("work"),
    )
    .expect("resources")
}

/// Builds a single-node located null document, mirroring the codec's located
/// handoff — the input whose miss (`as [$v]`, `.a`) yields owned null.
fn located_null(resources: &ResourceContext<'_>) -> EngineResult<'static> {
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder");
    let root = builder
        .add_node("test.null", AccountedSemanticNode::Null, None, resources)
        .expect("root");
    let document = builder.finish(root, resources).expect("document");
    let product = DocumentProduct::try_new(document, resources).expect("product");
    let handle = product.document().root_handle();
    EngineResult::Located(LocatedProduct::try_new(&product, handle).expect("located"))
}

/// Builds a single-node located bool document, mirroring the codec's located
/// handoff — the resolved-value input the identity forwarding tests need.
fn located_bool(resources: &ResourceContext<'_>) -> EngineResult<'static> {
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder");
    let root = builder
        .add_node("test.bool", AccountedSemanticNode::Bool(true), None, resources)
        .expect("root");
    let document = builder.finish(root, resources).expect("document");
    let product = DocumentProduct::try_new(document, resources).expect("product");
    let handle = product.document().root_handle();
    EngineResult::Located(LocatedProduct::try_new(&product, handle).expect("located"))
}

/// Builds a located array `[10, 20, 30]` (three integer items) and returns it
/// as the located root — the container the fan-out tests iterate.
fn located_int_array(resources: &ResourceContext<'_>) -> EngineResult<'static> {
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder");
    let mut items = Vec::new();
    for value in ["10", "20", "30"] {
        items.push(
            builder
                .add_node("test.int", AccountedSemanticNode::Integer(value), None, resources)
                .expect("item"),
        );
    }
    let root = builder
        .add_node(
            "test.array",
            AccountedSemanticNode::Array { item_role: "test.item" },
            None,
            resources,
        )
        .expect("root");
    for item in items {
        builder
            .add_occurrence(LocalOwnerRef::Node(root), "test.item", None, item, resources)
            .expect("edge");
    }
    let document = builder.finish(root, resources).expect("document");
    let product = DocumentProduct::try_new(document, resources).expect("product");
    let handle = product.document().root_handle();
    EngineResult::Located(LocatedProduct::try_new(&product, handle).expect("located"))
}

/// The integer text of a located engine result, for asserting fan-out order
/// without an encoder.
fn integer_of(result: &EngineResult<'_>) -> String {
    use jqf_data::{NumberView, ScalarView};
    let located = result.located().expect("located child");
    // The payload view: a tag LAYER's payload is what carries the scalar.
    let view = located.product().document().payload_view(located.node()).expect("view");
    match view.scalar().expect("scalar").expect("scalar present") {
        ScalarView::Number(NumberView::Integer(text)) => String::from(text),
        other => panic!("expected an integer child, got {other:?}"),
    }
}

fn each(optional: bool) -> StageStep {
    StageStep::new(StepAccess::Each, optional)
}

#[test]
fn identity_forwards_a_located_item_untouched() {
    let mut resources = resources();
    let input = located_bool(&resources);
    let mut stream = EngineRunStream::seed_stage(&[], 0, input);
    let first = stream.poll(&mut resources).expect("first poll");
    assert!(
        matches!(first, RunPoll::Item(EngineResult::Located(_))),
        "a located input must stay located through identity forwarding"
    );
    assert!(matches!(
        stream.poll(&mut resources).expect("completion"),
        RunPoll::Complete
    ));
}

#[test]
fn identity_forwards_an_owned_item_untouched() {
    let mut resources = resources();
    let mut stream = EngineRunStream::seed_stage(&[], 0, EngineResult::owned(Value::Bool(true)));
    match stream.poll(&mut resources).expect("first poll") {
        RunPoll::Item(EngineResult::Owned(Value::Bool(value))) => assert!(value),
        other => panic!("owned input must forward untouched, got {other:?}"),
    }
    assert!(matches!(
        stream.poll(&mut resources).expect("completion"),
        RunPoll::Complete
    ));
}

#[test]
fn last_leaf_finishes_the_stage_machine_in_place() {
    // A single-output residual produces its last leaf with no fan-out
    // remaining. The machine must finish on that leaf so the next poll
    // observes completion without walking an empty frame stack.
    let mut resources = resources();
    let mut machine = StageMachine::seed(&[], 0, EngineResult::owned(Value::Bool(true)));
    match machine.poll(&mut resources).expect("last leaf") {
        RunPoll::Item(EngineResult::Owned(Value::Bool(value))) => assert!(value),
        other => panic!("identity must emit the input, got {other:?}"),
    }
    assert!(machine.done, "the last leaf must finish the machine in place");
    assert!(
        machine.pending.is_none(),
        "a finished last leaf leaves no pending descent"
    );
    assert!(
        machine.frames.as_ref().is_none_or(Vec::is_empty),
        "a finished last leaf leaves no fan-out frames"
    );
    assert!(
        matches!(machine.poll(&mut resources).expect("completion"), RunPoll::Complete),
        "the next poll must observe the in-place finish"
    );
    assert!(
        matches!(machine.poll(&mut resources).expect("stable end"), RunPoll::Complete),
        "completion is a stable end, not a one-shot observation"
    );
}

#[test]
fn identity_run_leaves_working_memory_untouched() {
    // The single-slot machine's depth is one for an identity residual: the
    // pending task lives in a slot and the frame stack is never allocated, so
    // a complete identity run performs no heap or ledger traffic of its own.
    // This pins the per-value cost floor the NDJSON lanes depend on; a
    // fan-out's growth charge is the inverse case, asserted separately below.
    let mut resources = resources();
    let before = resources.snapshot().memory(MemoryCategory::Working).current();
    let mut stream = EngineRunStream::seed_stage(&[], 0, EngineResult::owned(Value::Null));
    let _ = stream.poll(&mut resources).expect("first poll");
    assert!(matches!(
        stream.poll(&mut resources).expect("completion"),
        RunPoll::Complete
    ));
    let after = resources.snapshot().memory(MemoryCategory::Working).current();
    assert_eq!(
        after, before,
        "an identity run must not touch Working memory ({before} -> {after})"
    );
}

#[test]
fn fan_out_charges_no_working_memory_for_the_frame_stack() {
    // The frame stack is a plain `Vec`: a `.[]` descent allocates it
    // WITHOUT a ledger charge, so Working memory stays flat across the first
    // frame.
    let mut resources = resources();
    let input = located_int_array(&resources);
    let residual = vec![each(false)];
    let before = resources.snapshot().memory(MemoryCategory::Working).current();
    let mut stream = EngineRunStream::seed_stage(&residual, 0, input);
    let _ = stream.poll(&mut resources).expect("first item");
    let after = resources.snapshot().memory(MemoryCategory::Working).current();
    assert_eq!(
        after, before,
        "a std-Vec frame stack is not ledger-charged ({before} -> {after})"
    );
}

#[test]
fn fan_out_emits_children_in_array_order_then_completes() {
    let mut resources = resources();
    let input = located_int_array(&resources);
    let residual = vec![each(false)];
    let mut stream = EngineRunStream::seed_stage(&residual, 0, input);
    let mut emitted = Vec::new();
    loop {
        match stream.poll(&mut resources).expect("poll") {
            RunPoll::Item(item) => emitted.push(integer_of(&item)),
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("credits are ample; no pause expected"),
        }
    }
    assert_eq!(emitted, ["10", "20", "30"]);
}

/// Located `. + 1` takes the frame-free binary exit. Byte-identical to the
/// owned arm, including the typed arithmetic error.
#[test]
fn located_identity_plus_literal_matches_the_owned_arm() {
    let mut ledger = resources();
    let located = located_int_array(&ledger);
    let owned = owned_int_array(&["10", "20", "30"], &ledger);
    let loc = drive_located(".[] | . + 1", located, &mut ledger).expect("located");
    let own = drive_source(".[] | . + 1", owned, &mut ledger).expect("owned");
    assert_eq!(loc, own);
    assert_eq!(loc, "11\n21\n31\n");

    let mut ledger = resources();
    let located = located_bool(&ledger);
    let loc_err = drive_located(". + 1", located, &mut ledger).expect_err("bool + 1");
    let own_err = drive_source(". + 1", Value::Bool(true), &mut ledger).expect_err("owned bool + 1");
    assert_eq!(alloc::format!("{loc_err:?}"), alloc::format!("{own_err:?}"));
}

/// `. + 1` is classified as identity × literal after fuse, so the frame-free
/// exit does not re-walk the arena per evaluation.
#[test]
fn plus_literal_is_the_identity_literal_binary_shape() {
    use crate::program::BinaryShape;
    let ledger = resources();
    let compiled = compiled_for_execute(". + 1", &ledger);
    let found = compiled.arena().iter().any(|node| {
        matches!(
            node,
            crate::program::ProgramNode::Binary {
                shape: BinaryShape::IdentityLiteral,
                ..
            }
        )
    });
    assert!(found, "`. + 1` must mark IdentityLiteral");
}

/// `{id: .id}` is a step-less string-literal key. `{(.k): 1}` is not.
#[test]
fn static_object_key_is_marked_only_for_literal_keys() {
    let ledger = resources();
    let static_prog = compiled_for_execute("{id: .id}", &ledger);
    let marked = static_prog.arena().iter().any(|node| match node {
        crate::program::ProgramNode::ConstructObject { members } => {
            members.len() == 1 && members[0].static_key.as_deref() == Some("id")
        }
        _ => false,
    });
    assert!(marked, "`{{id: .id}}` must mark static_key");

    let dynamic = compiled_for_execute("{(.k): 1}", &ledger);
    let unmarked = dynamic.arena().iter().any(|node| match node {
        crate::program::ProgramNode::ConstructObject { members } => {
            members.len() == 1 && members[0].static_key.is_none()
        }
        _ => false,
    });
    assert!(unmarked, "`{{(.k): 1}}` must keep a dynamic key");
}

/// `{id, name}` is a proven single combination; `{id: .[]}` is not.
#[test]
fn single_combination_object_is_conservative() {
    use crate::exec::graph_at_most_one;
    let ledger = resources();
    let single = compiled_for_execute("{id, name}", &ledger);
    let proven = single.arena().iter().any(|node| match node {
        crate::program::ProgramNode::ConstructObject { members } => members.iter().all(|member| {
            graph_at_most_one(single.arena(), member.key) && graph_at_most_one(single.arena(), member.value)
        }),
        _ => false,
    });
    assert!(proven, "`{{id, name}}` must prove a single combination");

    let many = compiled_for_execute("{id: .[]}", &ledger);
    let unproven = many.arena().iter().any(|node| match node {
        crate::program::ProgramNode::ConstructObject { members } => members
            .iter()
            .any(|member| !graph_at_most_one(many.arena(), member.value)),
        _ => false,
    });
    assert!(unproven, "`{{id: .[]}}` must keep the clone path");
}

/// `{id, name}` over a two-key object is byte-identical on located and owned.
#[test]
fn static_object_construction_matches_owned_and_located() {
    let mut ledger = resources();
    let located = located_value(
        &TestValue::Obj(&[("id", TestValue::Int("1")), ("name", TestValue::Int("2"))]),
        &ledger,
    );
    let owned = {
        let mut object = jqf_data::ObjectBuilder::try_with_capacity(2).expect("builder");
        object
            .try_insert_last(
                jqf_data::ObjectKey::try_from_str("id").expect("id"),
                Value::Number(jqf_data::Number::try_json_literal("1").expect("1")),
            )
            .expect("id");
        object
            .try_insert_last(
                jqf_data::ObjectKey::try_from_str("name").expect("name"),
                Value::Number(jqf_data::Number::try_json_literal("2").expect("2")),
            )
            .expect("name");
        Value::Object(object.try_finish().expect("finish"))
    };
    let loc = drive_located("{id, name}", located, &mut ledger).expect("located");
    let own = drive_source("{id, name}", owned, &mut ledger).expect("owned");
    assert_eq!(loc, own);
    assert_eq!(loc, "{\"id\":1,\"name\":2}\n");
}

/// An owned tagged array of integers (a `!list` tag wrapping an array
/// payload).
fn owned_tagged_array(tag: &str, elements: &[&str], _resources: &ResourceContext<'_>) -> Value {
    let mut array = jqf_data::Array::try_new().expect("array");
    for text in elements {
        array.try_push(int_value(text)).expect("push");
    }
    let tag = jqf_data::TagId::try_new_unaccounted(tag).expect("tag");
    Value::try_tagged(tag, Value::Array(array)).expect("tagged")
}

/// An owned tagged object of integer members (a `!map` tag wrapping an
/// object payload).
fn owned_tagged_object(tag: &str, members: &[(&str, &str)], _resources: &ResourceContext<'_>) -> Value {
    let mut builder = jqf_data::ObjectBuilder::try_with_capacity(members.len()).expect("builder");
    for (key, value) in members {
        builder
            .try_insert_last(jqf_data::ObjectKey::try_from_str(key).expect("key"), int_value(value))
            .expect("insert");
    }
    let tag = jqf_data::TagId::try_new_unaccounted(tag).expect("tag");
    Value::try_tagged(tag, Value::Object(builder.try_finish().expect("finish"))).expect("tagged")
}

/// The canonical integer text of an OWNED integer engine result.
fn owned_integer_text(result: &EngineResult<'_>) -> String {
    let EngineResult::Owned(Value::Number(number)) = result else {
        panic!("expected an owned number, got {result:?}");
    };
    String::from(number.to_integer().expect("integer").as_str())
}

/// The canonical integer texts of one OWNED array engine result's elements.
fn owned_array_integer_texts(result: &EngineResult<'_>) -> Vec<String> {
    let EngineResult::Owned(Value::Array(array)) = result else {
        panic!("expected an owned array, got {result:?}");
    };
    let mut texts = Vec::new();
    for element in array {
        let Value::Number(number) = element else {
            panic!("expected a number element, got {element:?}");
        };
        texts.push(String::from(number.to_integer().expect("integer").as_str()));
    }
    texts
}

#[test]
fn fan_out_over_an_owned_tagged_array_iterates_the_payload() {
    // The defect: `owned_kind` is payload-transparent, so a `.[]`
    // over an owned tagged array pushes the fan-out frame over the WRAPPER;
    // `next_owned_child` must see through it too, or the frame hits the
    // internal non-container contract error instead of iterating the payload.
    let mut resources = resources();
    let input = EngineResult::owned(owned_tagged_array("!list", &["10", "20", "30"], &resources));
    let residual = vec![each(false)];
    let mut stream = EngineRunStream::seed_stage(&residual, 0, input);
    let mut emitted = Vec::new();
    loop {
        match stream.poll(&mut resources).expect("poll") {
            RunPoll::Item(item) => emitted.push(owned_integer_text(&item)),
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("credits are ample; no pause expected"),
        }
    }
    assert_eq!(
        emitted,
        ["10", "20", "30"],
        "a `.[]` over an owned tagged array must yield the payload's children"
    );
}

#[test]
fn fan_out_over_an_owned_tagged_object_iterates_the_payload_values() {
    let mut resources = resources();
    let input = EngineResult::owned(owned_tagged_object(
        "!map",
        &[("a", "10"), ("b", "20"), ("c", "30")],
        &resources,
    ));
    let residual = vec![each(false)];
    let mut stream = EngineRunStream::seed_stage(&residual, 0, input);
    let mut emitted = Vec::new();
    loop {
        match stream.poll(&mut resources).expect("poll") {
            RunPoll::Item(item) => emitted.push(owned_integer_text(&item)),
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("credits are ample; no pause expected"),
        }
    }
    assert_eq!(
        emitted,
        ["10", "20", "30"],
        "a `.[]` over an owned tagged object must yield the payload's values in order"
    );
}

#[test]
fn descent_over_an_owned_tagged_container_is_pre_order_self_first() {
    // `..` over an owned tagged array: the tagged container is the first
    // emission, then its payload's children — the same pre-order the
    // located walk keeps, with the tag surviving on the container itself.
    let mut resources = resources();
    let input = EngineResult::owned(owned_tagged_array("!list", &["10", "20"], &resources));
    let residual = vec![StageStep::new(StepAccess::Descend, false)];
    let mut stream = EngineRunStream::seed_stage(&residual, 0, input);
    let mut items = Vec::new();
    loop {
        match stream.poll(&mut resources).expect("poll") {
            RunPoll::Item(item) => items.push(item),
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("credits are ample; no pause expected"),
        }
    }
    assert_eq!(items.len(), 3, "self plus two children");
    let EngineResult::Owned(Value::Tagged { tag, payload }) = &items[0] else {
        panic!("the self emission must be the tagged container, got {:?}", items[0]);
    };
    assert_eq!(tag.as_str(), "!list", "the self emission keeps its tag");
    assert!(
        matches!((**payload).untagged(), Value::Array(_)),
        "the self emission's payload must be the array"
    );
    assert_eq!(owned_integer_text(&items[1]), "10");
    assert_eq!(owned_integer_text(&items[2]), "20");
}

/// The keyed static fast lane engages exactly for the shape it owns: a
/// bare `Key`/`Index` stage key graph. A general key graph (here a
/// `Binary`) must keep the general drive — the counter is the witness that
/// the two spellings actually differ in drive, so a future change that
/// silently widens or narrows the lane's shape fails here rather than
/// drifting.
#[allow(
    clippy::too_many_lines,
    reason = "one fixture drive per drive arm (static and general), so the engagement \
                  counter is pinned for both in one test"
)]
#[test]
fn the_keyed_static_fast_lane_engages_for_static_key_graphs_only() {
    use core::sync::atomic::Ordering;
    use jqf_builtins::registry::builtins::id::SORT_BY;

    let mut resources = resources();
    // An owned `[{"k": 2}, {"k": 1}, {"k": 3}]` subject.
    let input = {
        let mut array = jqf_data::Array::try_new().expect("array");
        for value in ["2", "1", "3"] {
            let mut object = jqf_data::ObjectBuilder::try_with_capacity(1).expect("builder");
            object
                .try_insert_last(
                    jqf_data::ObjectKey::try_from_str("k").expect("key"),
                    Value::Number(jqf_data::Number::try_json_literal(value).expect("number literal")),
                )
                .expect("insert");
            array
                .try_push(Value::Object(object.try_finish().expect("finish")))
                .expect("push");
        }
        EngineResult::owned(Value::Array(array))
    };

    // sort_by(.k) — the static shape: the lane must engage.
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("k", false)],
        }, // 0: .k
        ProgramNode::call(BuiltinOverloadId::new(SORT_BY), SemanticRevision::new(1), vec![nid(0)]), // 1: sort_by(.k)
    ];
    let before = crate::exec::KEYED_STATIC_ENGAGEMENTS.load(Ordering::Relaxed);
    let mut stream = EngineRunStream::seed(
        &nodes,
        nid(1),
        nid(1),
        0,
        0,
        &[],
        &[],
        &[],
        input.try_clone().expect("clone"),
        None,
    )
    .expect("test seed");
    let mut emitted = Vec::new();
    loop {
        match stream.poll(&mut resources).expect("poll") {
            RunPoll::Item(item) => emitted.push(item),
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("credits are ample; no pause expected"),
        }
    }
    assert_eq!(
        crate::exec::KEYED_STATIC_ENGAGEMENTS.load(Ordering::Relaxed),
        before + 1,
        "a static key graph must take the fast lane"
    );
    // The sorted answer is `[{"k":1}, {"k":2}, {"k":3}]`: the first
    // published element is the `k == 1` object.
    let first = emitted.into_iter().next().expect("one item");
    let EngineResult::Owned(first) = first else {
        panic!("sort_by must publish an owned array");
    };
    let Value::Array(emitted) = first.untagged() else {
        panic!("sort_by must publish an array");
    };
    let Value::Object(first_object) = emitted.get(0).expect("first").untagged() else {
        panic!("first element must be an object");
    };
    let first_k = first_object.get_index(0).expect("member").value();
    let Value::Number(number) = first_k.untagged() else {
        panic!("k must be a number");
    };
    assert_eq!(
        number.to_integer().expect("integer").as_str(),
        "1",
        "the fast lane must publish the same sorted order as the general drive"
    );

    // sort_by(.k + .k) — a Binary key graph: the general drive must stay.
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("k", false)],
        }, // 0: .k
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("k", false)],
        }, // 1: .k
        ProgramNode::Binary {
            op: BinaryKind::Add,
            left: nid(0),
            right: nid(1),
            shape: crate::program::BinaryShape::Framed,
        }, // 2: .k + .k
        ProgramNode::call(BuiltinOverloadId::new(SORT_BY), SemanticRevision::new(1), vec![nid(2)]), // 3: sort_by(.k + .k)
    ];
    let before = crate::exec::KEYED_STATIC_ENGAGEMENTS.load(Ordering::Relaxed);
    let mut stream = EngineRunStream::seed(
        &nodes,
        nid(3),
        nid(3),
        0,
        0,
        &[],
        &[],
        &[],
        input.try_clone().expect("clone"),
        None,
    )
    .expect("test seed");
    loop {
        match stream.poll(&mut resources).expect("poll") {
            RunPoll::Item(_) => {}
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("credits are ample; no pause expected"),
        }
    }
    assert_eq!(
        crate::exec::KEYED_STATIC_ENGAGEMENTS.load(Ordering::Relaxed),
        before,
        "a general key graph must keep the general drive"
    );
}

#[test]
fn cooperative_pause_resumes_inside_a_fan_out_preserving_order_and_completeness() {
    // Drive a `.[]` over `[10, 20, 30]` with a one-credit window, forcing a
    // cooperative pause between items; every resume must continue the frame
    // cursor exactly, emitting all three in order with no repeats or gaps.
    let mut resources = resources_with_credits(1);
    // Consume the priming credit so the first advance must pause.
    assert!(matches!(
        resources.admit_work_transition().expect("prime"),
        jqf_resource::WorkAdmission::Granted(_)
    ));
    let input = located_int_array(&resources);
    let residual = vec![each(false)];
    let mut stream = EngineRunStream::seed_stage(&residual, 0, input);
    let mut emitted = Vec::new();
    let mut guard = 0;
    loop {
        guard += 1;
        assert!(guard < 100, "the paused fan-out must make progress");
        match stream.poll(&mut resources).expect("poll") {
            RunPoll::Pending => {
                assert!(resources.try_begin_next_cooperative_entry(1).expect("resume"));
            }
            RunPoll::Item(item) => emitted.push(integer_of(&item)),
            RunPoll::Complete => break,
        }
    }
    assert_eq!(
        emitted,
        ["10", "20", "30"],
        "a paused-and-resumed fan-out must emit every child once, in order"
    );
}

#[test]
fn iterate_over_a_scalar_is_the_iterate_mismatch_class() {
    // `.[]` over a bool is the distinct iterate class, reported at the global
    // step index (base offset 0 here).
    let mut resources = resources();
    let input = located_bool(&resources);
    let residual = vec![each(false)];
    let mut stream = EngineRunStream::seed_stage(&residual, 0, input);
    let error = stream.poll(&mut resources).expect_err("iterating a scalar must fail");
    assert!(matches!(
        error,
        EngineRunError::IterateMismatch {
            step_index: 0,
            actual_type: ValueKind::Bool,
            ..
        }
    ));
}

#[test]
fn cooperative_pause_retains_the_continuation_and_resumes() {
    let mut resources = resources_with_credits(1);
    assert!(matches!(
        resources.admit_work_transition().expect("prime"),
        jqf_resource::WorkAdmission::Granted(_)
    ));
    let mut stream = EngineRunStream::seed_stage(&[], 0, EngineResult::owned(Value::Bool(true)));
    assert!(
        matches!(stream.poll(&mut resources).expect("pause"), RunPoll::Pending),
        "an exhausted work slice must pause instead of emitting"
    );
    assert!(resources.try_begin_next_cooperative_entry(64).expect("resume"));
    match stream.poll(&mut resources).expect("post-resume item") {
        RunPoll::Item(EngineResult::Owned(Value::Bool(value))) => assert!(value),
        other => panic!("the retained continuation must yield its item, got {other:?}"),
    }
    assert!(matches!(
        stream.poll(&mut resources).expect("completion"),
        RunPoll::Complete
    ));
}

#[test]
fn emitted_prefix_is_published_before_a_trailing_failure_surfaces() {
    // Test-only arm: no compiled program produces `EmitThenFail`, so the
    // test constructs one and drains it through the real driver. The prefix
    // item must be emitted before ANY armed failure surfaces.
    let mut resources = resources();
    // The atomic emit-then-fail arm belongs to the fast-path stage machine, so
    // the test drives a `StageMachine` directly (the enum wrapper exposes only
    // `poll`).
    let mut stream = StageMachine::seed(&[], 0, EngineResult::owned(Value::Null));
    let _ = stream.poll(&mut resources).expect("drain seed");
    let _ = stream.poll(&mut resources).expect("seed complete");
    let outcome = StepOutcome::EmitThenFail {
        item: EngineResult::owned(Value::Bool(true)),
        error: EngineError::Internal("test-only atomic emit-then-fail"),
    };
    match stream.absorb(outcome) {
        RunStep::Emit(EngineResult::Owned(Value::Bool(value))) => assert!(value),
        other => panic!("the prefix item must be emitted first, got {other:?}"),
    }
    let error = stream
        .resume(&mut resources)
        .expect_err("the trailing failure must surface after its prefix");
    assert!(matches!(error, EngineRunError::Codec(_)));
}

#[test]
fn try_run_streams_a_resolved_value() {
    let program = stage_program(Vec::new());
    let run = run_stage(
        &program,
        CodecInputOutcome::Result(EngineResult::owned(Value::Bool(true))),
    )
    .expect("run");
    assert!(matches!(
        run,
        EngineRun::Stream {
            input: RunInput::Resolved,
            ..
        }
    ));
}

#[test]
fn try_run_maps_missing_to_a_missing_tagged_null_stream() {
    let resources = resources();
    let outcome = missing_outcome(&resources);
    // A `?` on the step is irrelevant to a missing path: still a null stream.
    let program = stage_program(steps(&[true]));
    let run = run_stage(&program, outcome).expect("run");
    assert!(matches!(
        run,
        EngineRun::Stream {
            input: RunInput::Missing,
            ..
        }
    ));
}

#[test]
fn try_run_maps_null_mismatch_to_a_missing_tagged_stream_under_either_flag() {
    let resources = resources();
    // Null precedence runs before the flag check (`{"a":null} | .a.b?`
    // → null): a mismatch-against-null is a Missing-tagged null stream
    // whether the failing step carries `?` or not — the flag arm is never
    // reached, so the disposition cannot depend on it. The flag's own half
    // of the law (Suppressed vs Pushdown on a non-null mismatch) is
    // `try_run_suppresses_a_non_null_mismatch_at_a_flagged_step`.
    for flagged in [false, true] {
        let outcome = CodecInputOutcome::TypeMismatch {
            authority: missing_authority(&resources),
            step_index: 2,
            actual_type: ValueKind::Null,
            hint: None,
        };
        let program = stage_program(steps(&[false, false, flagged]));
        let run = run_stage(&program, outcome).expect("run");
        assert!(
            matches!(
                run,
                EngineRun::Stream {
                    input: RunInput::Missing,
                    ..
                }
            ),
            "a null mismatch maps to a Missing stream with the step {flagged}"
        );
    }
}

#[test]
fn try_run_maps_non_null_mismatch_to_the_typed_abort_arm() {
    let resources = resources();
    let outcome = CodecInputOutcome::TypeMismatch {
        authority: missing_authority(&resources),
        step_index: 3,
        actual_type: ValueKind::Number,
        hint: None,
    };
    let program = stage_program(steps(&[false, false, false, false]));
    let run = run_stage(&program, outcome).expect("run");
    let EngineRun::Pushdown(EngineRunError::TypeMismatch {
        step_index,
        actual_type,
        key,
        hint: None,
    }) = run
    else {
        panic!("pushed-down mismatch");
    };
    assert_eq!(step_index, 3);
    assert_eq!(actual_type, ValueKind::Number);
    // The accessor is rendered here, where the failing step is still in hand.
    assert_eq!(key, "string (\"k\")");
}

#[test]
fn try_run_suppresses_a_non_null_mismatch_at_a_flagged_step() {
    let resources = resources();
    let outcome = CodecInputOutcome::TypeMismatch {
        authority: missing_authority(&resources),
        step_index: 1,
        actual_type: ValueKind::Number,
        hint: None,
    };
    // The `?` sits on the failing step itself (`{"a":5} | .a.b?`): suppressed.
    let program = stage_program(steps(&[false, true]));
    let run = run_stage(&program, outcome).expect("run");
    assert!(matches!(run, EngineRun::Suppressed));
}

#[test]
fn try_run_declines_a_pushed_down_mismatch_at_a_non_static_step() {
    // Every requirement lowering rejects `.[]` from the codec prefix, so a
    // codec-reported mismatch can never land on one. If one ever did, the
    // index message would have no accessor to interpolate — so the arm is a
    // named contract violation rather than a fabricated message.
    let resources = resources();
    let outcome = CodecInputOutcome::TypeMismatch {
        authority: missing_authority(&resources),
        step_index: 0,
        actual_type: ValueKind::Number,
        hint: None,
    };
    let program = stage_program(vec![each(false)]);
    assert!(run_stage(&program, outcome).is_err());

    // A flagged `.[]?` over the same scalar still suppresses to zero items:
    // suppression is decided before the class is rendered.
    let outcome = CodecInputOutcome::TypeMismatch {
        authority: missing_authority(&resources),
        step_index: 0,
        actual_type: ValueKind::Number,
        hint: None,
    };
    let flagged = stage_program(vec![each(true)]);
    let run = run_stage(&flagged, outcome).expect("run");
    assert!(matches!(run, EngineRun::Suppressed));
}

#[test]
fn try_run_exact_step_does_not_suppress_an_earlier_mismatch() {
    let resources = resources();
    let outcome = CodecInputOutcome::TypeMismatch {
        authority: missing_authority(&resources),
        step_index: 0,
        actual_type: ValueKind::Number,
        hint: None,
    };
    // `5 | .a.b?`: the mismatch is at step 0, but only step 1 is flagged, so
    // the exact-step law aborts rather than suppressing.
    let program = stage_program(steps(&[false, true]));
    let run = run_stage(&program, outcome).expect("run");
    assert!(matches!(
        run,
        EngineRun::Pushdown(EngineRunError::TypeMismatch {
            step_index: 0,
            actual_type: ValueKind::Number,
            ..
        })
    ));
}

// --- Graph machine (Choice / FlatMap arms) ---------------------------------

/// A small located-document spec for the graph tests: an integer leaf, a
/// bool leaf, an array of specs, or a keyed object of nested specs.
enum TestValue {
    Int(&'static str),
    Bool(bool),
    Arr(&'static [TestValue]),
    Obj(&'static [(&'static str, TestValue)]),
}

/// Adds `value` to `builder`, returning its node id (recursion at build time
/// only — the executor itself never recurses).
fn build_value(
    builder: &mut AccountedDocumentBuilder<'static>,
    value: &TestValue,
    resources: &ResourceContext<'_>,
) -> NodeId {
    match value {
        TestValue::Int(text) => builder
            .add_node("test.int", AccountedSemanticNode::Integer(text), None, resources)
            .expect("int node"),
        TestValue::Bool(value) => builder
            .add_node("test.bool", AccountedSemanticNode::Bool(*value), None, resources)
            .expect("bool node"),
        TestValue::Arr(items) => {
            let children: Vec<NodeId> = items.iter().map(|item| build_value(builder, item, resources)).collect();
            let node = builder
                .add_node(
                    "test.array",
                    AccountedSemanticNode::Array { item_role: "test.item" },
                    None,
                    resources,
                )
                .expect("array node");
            for child in children {
                builder
                    .add_occurrence(LocalOwnerRef::Node(node), "test.item", None, child, resources)
                    .expect("item");
            }
            node
        }
        TestValue::Obj(members) => {
            let node = builder
                .add_node(
                    "test.object",
                    AccountedSemanticNode::Object {
                        member_role: "test.member",
                    },
                    None,
                    resources,
                )
                .expect("object node");
            for (key, child) in *members {
                let child_node = build_value(builder, child, resources);
                builder
                    .add_occurrence(
                        LocalOwnerRef::Node(node),
                        "test.member",
                        Some(AccountedOccurrenceKey::Text(key)),
                        child_node,
                        resources,
                    )
                    .expect("member");
            }
            node
        }
    }
}

/// Builds a located root document from a [`TestValue`] spec.
fn located_value(value: &TestValue, resources: &ResourceContext<'_>) -> EngineResult<'static> {
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder");
    let root = build_value(&mut builder, value, resources);
    let document = builder.finish(root, resources).expect("document");
    let product = DocumentProduct::try_new(document, resources).expect("product");
    let handle = product.document().root_handle();
    EngineResult::Located(LocatedProduct::try_new(&product, handle).expect("located"))
}

fn nid(index: usize) -> ProgramNodeId {
    ProgramNodeId::from_index(index).expect("node id")
}

/// Drives a graph stream to completion, collecting each emitted integer.
fn drive_ints(
    nodes: &[ProgramNode],
    root: ProgramNodeId,
    input: EngineResult<'_>,
    resources: &mut ResourceContext<'_>,
) -> Vec<String> {
    let mut stream = EngineRunStream::seed(nodes, root, root, 0, 0, &[], &[], &[], input, None).expect("test seed");
    let mut emitted = Vec::new();
    loop {
        match stream.poll(resources).expect("poll") {
            RunPoll::Item(item) => emitted.push(integer_of(&item)),
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("ample credits; no pause expected"),
        }
    }
    emitted
}

/// Collects a run's emitted items as owned-string renderings (a `Value::String`
/// yields its text), for the try/catch tests whose handler outputs are strings.
fn drive_strings(
    nodes: &[ProgramNode],
    root: ProgramNodeId,
    input: EngineResult<'_>,
    resources: &mut ResourceContext<'_>,
) -> Vec<String> {
    let mut stream = EngineRunStream::seed(nodes, root, root, 0, 0, &[], &[], &[], input, None).expect("test seed");
    let mut emitted = Vec::new();
    loop {
        match stream.poll(resources).expect("poll") {
            RunPoll::Item(EngineResult::Owned(Value::String(text))) => {
                emitted.push(String::from(text.as_str()));
            }
            RunPoll::Item(other) => panic!("expected an owned string, got {other:?}"),
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("ample credits; no pause expected"),
        }
    }
    emitted
}

#[test]
fn catchless_try_swallows_a_body_error_and_completes() {
    // `try .a` over the number 5: `.a` raises an index mismatch, the catchless
    // barrier swallows it, and the run completes with zero items.
    let mut resources = resources();
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("a", false)],
        },
        ProgramNode::Try {
            body: nid(0),
            handler: None,
        },
    ];
    let items = drive_strings(&nodes, nid(1), EngineResult::owned(int_value("5")), &mut resources);
    assert!(items.is_empty(), "a catchless try swallows the body error");
}

/// A caught raise records the barrier depth BEFORE the pop loop: reading
/// `frames.len() - index` after popping lands on exactly `index`, so the
/// diag record's `caught` field would be 0 for every caught raise.
/// `try .a catch .` over `null` raises at `.a` with the
/// barrier and its body frames on the stack, so `caught` must be ≥ 1.
#[test]
fn a_caught_raise_records_the_barrier_depth() {
    use core::cell::RefCell;
    use jqf_resource::diag::{DiagnosticRecord, DiagnosticSink};

    // The narrowest sink: retain every record's `caught` depth.
    #[derive(Default)]
    struct CaughtSink(RefCell<Vec<Option<u32>>>);
    impl DiagnosticSink for CaughtSink {
        fn record(&self, record: DiagnosticRecord<'_>) {
            self.0.borrow_mut().push(record.caught);
        }
    }

    let sink = CaughtSink::default();
    let mut resources = ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(4096).expect("work"),
    )
    .expect("resources")
    .with_diagnostics(&sink);

    let compiled = compiled_for_execute("try .a catch .", &resources);
    let mut stream = match compiled
        .try_run_whole_value(
            CodecInputOutcome::Result(EngineResult::owned(Value::Number(jqf_data::Number::integer(
                jqf_data::Integer::from_i64(1),
            )))),
            &resources,
        )
        .expect("run seeds")
    {
        EngineRun::Stream { stream, .. } => stream,
        other => panic!("expected a stream: {other:?}"),
    };
    loop {
        match stream.poll(&mut resources).expect("poll") {
            RunPoll::Item(_) => {}
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("ample credits; no pause expected"),
        }
    }
    let depths = sink.0.borrow();
    assert!(
        depths.iter().any(|depth| *depth >= Some(1)),
        "a caught raise must record a barrier depth >= 1; got {depths:?}"
    );
}

#[test]
fn catch_handler_receives_the_message_string() {
    // `try .a catch .` over 5: the caught value is the exact index message,
    // handed to the identity handler and emitted.
    let mut resources = resources();
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("a", false)],
        },
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: Vec::new(),
        },
        ProgramNode::Try {
            body: nid(0),
            handler: Some(nid(1)),
        },
    ];
    let items = drive_strings(&nodes, nid(2), EngineResult::owned(int_value("5")), &mut resources);
    assert_eq!(items, ["Cannot index number with string (\"a\")"]);
}

/// An OWNED bytes value must be reported with the honest kind word in its
/// mismatch message: `owned_kind` is `Value::kind()`
/// (payload-transparent), so an owned value and the same value reached
/// through the located view agree on `string`, the nearest JSON word for
/// bytes.
#[test]
fn an_owned_bytes_value_reports_its_honest_kind_in_a_mismatch_message() {
    let mut resources = resources();
    let bytes = Value::try_bytes(b"\x00\x01\x02").expect("fixture bytes");
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("a", false)],
        },
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: Vec::new(),
        },
        ProgramNode::Try {
            body: nid(0),
            handler: Some(nid(1)),
        },
    ];
    let items = drive_strings(&nodes, nid(2), EngineResult::owned(bytes), &mut resources);
    assert_eq!(items, ["Cannot index string with string (\"a\")"]);
}

#[test]
fn a_machine_error_is_not_catch_eligible_and_tears_through() {
    // A `Codec` machine error (cancellation, deadline, ledger rejection,
    // internal contract) is NOT catchable — a `try` barrier never swallows it.
    let codec = EngineRunError::allocation_failure();
    assert!(!codec.is_catch_eligible());
    // `halt`/`halt_error` are process-level: NOT catch-eligible and NOT
    // suppressed by `?`, exactly like the machine channel.
    assert!(
        !EngineRunError::Halt {
            status: 0,
            message: None,
        }
        .is_catch_eligible()
    );
    // A raised value and every typed semantic arm ARE catch-eligible.
    assert!(EngineRunError::Raised(Value::Null).is_catch_eligible());
    assert!(
        EngineRunError::TypeMismatch {
            step_index: 0,
            actual_type: ValueKind::Number,
            key: String::from("string (\"a\")"),
            hint: None,
        }
        .is_catch_eligible()
    );
}

#[test]
fn a_try_does_not_catch_a_sibling_and_right_error() {
    let mut resources = resources();
    let input = EngineResult::owned(int_value("5"));
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: Vec::new(),
        },
        ProgramNode::Try {
            body: nid(0),
            handler: None,
        },
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("a", false)],
        },
        ProgramNode::Logical {
            operator: LogicalOp::And,
            left: nid(1),
            right: nid(2),
        },
    ];
    let mut stream =
        EngineRunStream::seed(&nodes, nid(3), nid(3), 0, 0, &[], &[], &[], input, None).expect("test seed");
    assert!(matches!(
        stream.poll(&mut resources).expect_err("sibling error escapes the try"),
        EngineRunError::TypeMismatch { .. }
    ));
}

#[test]
fn a_try_does_not_catch_a_sibling_product_right_error() {
    let mut resources = resources();
    let input = EngineResult::owned(int_value("5"));
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: Vec::new(),
        },
        ProgramNode::Try {
            body: nid(0),
            handler: None,
        },
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("a", false)],
        },
        ProgramNode::Binary {
            op: BinaryKind::Add,
            left: nid(2),
            right: nid(1),
            shape: crate::program::BinaryShape::Framed,
        },
    ];
    let mut stream =
        EngineRunStream::seed(&nodes, nid(3), nid(3), 0, 0, &[], &[], &[], input, None).expect("test seed");
    assert!(matches!(
        stream
            .poll(&mut resources)
            .expect_err("sibling error escapes the try in +"),
        EngineRunError::TypeMismatch { .. }
    ));
}

#[test]
fn a_try_does_not_catch_a_sibling_object_value_error() {
    let mut resources = resources();
    let input = located_value(&TestValue::Int("5"), &resources);
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: Vec::new(),
        },
        ProgramNode::Try {
            body: nid(0),
            handler: None,
        },
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("a", false)],
        },
        ProgramNode::Stage {
            start: StageStart::Literal(Value::try_string("x").expect("x")),
            steps: Vec::new(),
        },
        ProgramNode::Stage {
            start: StageStart::Literal(Value::try_string("y").expect("y")),
            steps: Vec::new(),
        },
        ProgramNode::ConstructObject {
            members: vec![
                ObjectMemberNode {
                    key: nid(3),
                    value: nid(1),
                    static_key: None,
                },
                ObjectMemberNode {
                    key: nid(4),
                    value: nid(2),
                    static_key: None,
                },
            ],
        },
    ];
    let mut stream =
        EngineRunStream::seed(&nodes, nid(5), nid(5), 0, 0, &[], &[], &[], input, None).expect("test seed");
    assert!(matches!(
        stream
            .poll(&mut resources)
            .expect_err("sibling error escapes the try in object"),
        EngineRunError::TypeMismatch { .. }
    ));
}

#[test]
fn a_try_does_not_catch_a_sibling_reduce_update_error() {
    let mut resources = resources();
    let input = EngineResult::owned(int_value("5"));
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: Vec::new(),
        },
        ProgramNode::Try {
            body: nid(0),
            handler: None,
        },
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: Vec::new(),
        },
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("a", false)],
        },
        ProgramNode::Reduce {
            source: nid(1),
            slot: 0,
            init: nid(2),
            update: nid(3),
            keyed_collect: None,
        },
    ];
    let mut stream =
        EngineRunStream::seed(&nodes, nid(4), nid(4), 0, 1, &[], &[], &[], input, None).expect("test seed");
    assert!(matches!(
        stream
            .poll(&mut resources)
            .expect_err("sibling error escapes the try in reduce"),
        EngineRunError::TypeMismatch { .. }
    ));
}

#[test]
fn a_try_catch_handler_does_not_catch_a_sibling_and_right_error() {
    let mut resources = resources();
    let input = EngineResult::owned(int_value("5"));
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: Vec::new(),
        },
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: Vec::new(),
        },
        ProgramNode::Try {
            body: nid(0),
            handler: Some(nid(1)),
        },
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("a", false)],
        },
        ProgramNode::Logical {
            operator: LogicalOp::And,
            left: nid(2),
            right: nid(3),
        },
    ];
    let mut stream =
        EngineRunStream::seed(&nodes, nid(4), nid(4), 0, 0, &[], &[], &[], input, None).expect("test seed");
    assert!(matches!(
        stream
            .poll(&mut resources)
            .expect_err("sibling error escapes try-catch"),
        EngineRunError::TypeMismatch { .. }
    ));
}

#[test]
fn try_body_error_escapes_a_downstream_pipeline() {
    let mut resources = resources();
    let input = located_value(&TestValue::Obj(&[("a", TestValue::Int("1"))]), &resources);
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("a", false)],
        },
        ProgramNode::Try {
            body: nid(0),
            handler: None,
        },
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("b", false)],
        },
        ProgramNode::FlatMap {
            upstream: nid(1),
            body: nid(2),
        },
    ];
    let mut stream =
        EngineRunStream::seed(&nodes, nid(3), nid(3), 0, 0, &[], &[], &[], input, None).expect("test seed");
    assert!(matches!(
        stream
            .poll(&mut resources)
            .expect_err("downstream error escapes the try, as in `try .a | .b`"),
        EngineRunError::TypeMismatch { .. }
    ));
}

/// Compiles `program` and drives one whole-value run over owned `null`,
/// returning the number of published items or the first error.
///
/// The whole-value run is the `-n` shape: no codec pushed anything down, so
/// the graph runs the whole program over the synthesized null input — which
/// is exactly the shape the fuzzer's forced floor (`[.][0] | (P)`) would
/// reach through the ordinary SDK.
fn drive_null(program: &str, resources: &mut ResourceContext<'_>) -> Result<usize, EngineRunError> {
    let compiled = compiled_for_execute(program, resources);
    let mut stream = match compiled
        .try_run_whole_value(CodecInputOutcome::Result(EngineResult::owned(Value::Null)), resources)
        .expect("run seeds")
    {
        EngineRun::Stream { stream, .. } => stream,
        EngineRun::Suppressed => return Ok(0),
        EngineRun::Pushdown(error) => return Err(error),
        EngineRun::ReboundWhole => panic!("whole-value run must not rebound"),
    };
    let mut items = 0;
    loop {
        match stream.poll(resources) {
            Ok(RunPoll::Item(_)) => items += 1,
            Ok(RunPoll::Complete) => return Ok(items),
            Ok(RunPoll::Pending) => {}
            Err(error) => return Err(error),
        }
    }
}

/// `drive_null` collecting each published value's rendered JSON, for tests
/// that must pin WHICH values a swallowing barrier lets through, not just
/// how many.
fn drive_null_values(
    program: &str,
    resources: &mut ResourceContext<'_>,
) -> Result<alloc::vec::Vec<alloc::string::String>, EngineRunError> {
    let compiled = compiled_for_execute(program, resources);
    let mut stream = match compiled
        .try_run_whole_value(CodecInputOutcome::Result(EngineResult::owned(Value::Null)), resources)
        .expect("run seeds")
    {
        EngineRun::Stream { stream, .. } => stream,
        EngineRun::Suppressed => return Ok(alloc::vec::Vec::new()),
        EngineRun::Pushdown(error) => return Err(error),
        EngineRun::ReboundWhole => panic!("whole-value run must not rebound"),
    };
    let mut out = alloc::vec::Vec::new();
    loop {
        match stream.poll(resources) {
            Ok(RunPoll::Item(EngineResult::Owned(value))) => {
                out.push(jqf_builtins::semantics::render::to_json(&value)?);
            }
            Ok(RunPoll::Item(EngineResult::Located(_))) => {
                return Err(super::internal_contract("null-input drive publishes located"));
            }
            Ok(RunPoll::Complete) => return Ok(out),
            Ok(RunPoll::Pending) => {}
            Err(error) => return Err(error),
        }
    }
}

/// Short-circuit operand evaluation: an empty stream cancels a raising
/// sibling before it is ever reached, a raising stream raises before an
/// empty value cancels, and an object key is validated only once its value
/// produces.
#[test]
fn an_empty_operand_cancels_a_sibling_raise() {
    let mut resources = resources();
    // `IN(s; t)` is `any(s == t; .)`; the second argument is the RIGHT
    // (outer) operand of `==`, evaluated first — so an empty stream means
    // the raising value is never reached and `any` answers `false`.
    assert!(matches!(drive_null("IN(error; empty)", &mut resources), Ok(1)));
    // The mirror: the raising stream is the outer operand, so it raises
    // before the empty value can cancel it.
    assert!(matches!(
        drive_null("IN(empty; error)", &mut resources),
        Err(EngineRunError::Raised(_))
    ));
    // `{(.): empty}` — the key is validated only when its value produces,
    // so an empty member answers nothing even with a non-string key.
    assert!(matches!(drive_null("{(.): empty}", &mut resources), Ok(0)));
}

/// `match`/`capture`/`scan` are defined as `_match_impl(...) | .[]`, so
/// inside path mode the EACH over the match array always fails the register
/// check: the `Invalid path expression near attempt to iterate through
/// [...]` raise happens even when the pattern matches nothing, while a
/// native no-match outside path mode stays a silent empty and the
/// enclosing `del`/`path`/`pick` answer.
#[test]
fn a_no_match_regex_iterator_raises_inside_path_mode() {
    let mut resources = resources();
    let mut drive = |program: &str| -> Result<Vec<String>, EngineRunError> {
        let compiled = compiled_for_execute(program, &resources);
        let input = Value::try_string("hello").expect("hello");
        let mut stream = match compiled
            .try_run_whole_value(CodecInputOutcome::Result(EngineResult::owned(input)), &resources)
            .expect("run seeds")
        {
            EngineRun::Stream { stream, .. } => stream,
            EngineRun::Suppressed => return Ok(Vec::new()),
            EngineRun::Pushdown(error) => return Err(error),
            EngineRun::ReboundWhole => panic!("whole-value run must not rebound"),
        };
        let mut out = Vec::new();
        loop {
            match stream.poll(&mut resources) {
                Ok(RunPoll::Item(EngineResult::Owned(value))) => {
                    out.push(jqf_builtins::semantics::render::to_json(&value)?);
                }
                Ok(RunPoll::Item(EngineResult::Located(_))) => {
                    return Err(super::internal_contract("path-mode drive publishes owned"));
                }
                Ok(RunPoll::Complete) => return Ok(out),
                Ok(RunPoll::Pending) => panic!("credits are ample; no pause expected"),
                Err(error) => return Err(error),
            }
        }
    };
    // The family's reproducers: the no-match iterator inside path mode
    // must RAISE (exit 5), not answer silently.
    for program in [
        "del(match(\"k\"))",
        "del(scan(\"k\"))",
        "del(capture(\"k\"))",
        "path(match(\"k\"))",
        "path(match(\"k\"; \"g\"))",
        "pick(match(\"k\"))",
    ] {
        assert!(
            matches!(drive(program), Err(EngineRunError::Raised(_))),
            "{program} must raise inside path mode"
        );
    }
    // A MATCHING pattern raises too, and a no-match OUTSIDE path mode
    // stays a silent empty.
    assert!(matches!(drive("del(match(\"h\"))"), Err(EngineRunError::Raised(_))));
    assert!(matches!(drive("match(\"k\")"), Ok(items) if items.is_empty()));
    // A MATCHING pattern outside path mode still publishes the match.
    assert!(matches!(drive("match(\"h\")"), Ok(items) if items.len() == 1));
}

/// The path-mode `try` catches a break MARKER: `label $l0 | path(try
/// (break $l0) catch .)` runs the handler and its emission fails the
/// register check (`Invalid path expression with result …`), where the
/// marker exemption let the break reach its label and the path-mode call
/// answered empty.
#[test]
fn the_path_mode_try_catches_a_break_marker() {
    let mut resources = resources();
    for program in [
        "label $l0 | path(try (break $l0) catch \"caught\")",
        "label $l0 | path(try (break $l0) catch .)",
    ] {
        assert!(
            matches!(drive_null(program, &mut resources), Err(EngineRunError::Raised(_))),
            "{program} must raise inside path mode"
        );
    }
    // A break with NO intervening try still reaches its label.
    assert!(matches!(
        drive_null("label $l0 | path(break $l0)", &mut resources),
        Ok(0)
    ));
    // Outside path mode the marker is caught and the handler emits.
    assert!(matches!(
        drive_null("label $l0 | try (break $l0) catch \"caught\"", &mut resources),
        Ok(1)
    ));
}

/// The multi-member array-pattern frame binds RIGHT-to-left: `"a" as
/// [$v0, $v1]` fails at `.[1]`, so a `try` handler `catch .` renders
/// `Cannot index string with number (1)`.
#[test]
fn the_array_pattern_frame_binds_right_to_left() {
    let mut resources = resources();
    let mut caught = |program: &str| -> String {
        let compiled = compiled_for_execute(program, &resources);
        let mut stream = match compiled
            .try_run_whole_value(CodecInputOutcome::Result(EngineResult::owned(Value::Null)), &resources)
            .expect("run seeds")
        {
            EngineRun::Stream { stream, .. } => stream,
            other => panic!("expected a stream: {other:?}"),
        };
        match stream.poll(&mut resources).expect("poll") {
            RunPoll::Item(EngineResult::Owned(value)) => {
                jqf_builtins::semantics::render::to_json(&value).expect("renders")
            }
            other => panic!("expected one caught message: {other:?}"),
        }
    };
    assert_eq!(
        caught("try (\"a\" as [$v0, $v1] | $v0) catch ."),
        "\"Cannot index string with number (1)\""
    );
    assert_eq!(
        caught("try (\"a\" as [$v0, $v1, $v2] | $v0) catch ."),
        "\"Cannot index string with number (2)\""
    );
    // A successful multi-binder still binds every name.
    let compiled = compiled_for_execute("[1,2] as [$v0, $v1] | [$v0, $v1]", &resources);
    let mut stream = match compiled
        .try_run_whole_value(CodecInputOutcome::Result(EngineResult::owned(Value::Null)), &resources)
        .expect("run seeds")
    {
        EngineRun::Stream { stream, .. } => stream,
        other => panic!("expected a stream: {other:?}"),
    };
    match stream.poll(&mut resources).expect("poll") {
        RunPoll::Item(EngineResult::Owned(value)) => {
            assert_eq!(
                jqf_builtins::semantics::render::to_json(&value).expect("renders"),
                "[1,2]"
            );
        }
        other => panic!("expected the array: {other:?}"),
    }
}

/// The path-mode fold's register law: `length` on a number returns a FRESH
/// number, so a fold whose final state went through `length` fails the
/// path-mode emission check (`del(reduce (10) as $v0 (.; length))` is
/// `Invalid path expression with result …`), where the length pass-through
/// let the input's own handle track and the empty-path deletion ran. An
/// identity update still publishes.
#[test]
fn a_fold_final_state_through_length_fails_the_emission_check() {
    let mut resources = resources();
    for program in [
        "del(reduce (10) as $v0 (.; length))",
        "path(reduce (10) as $v0 (.; length))",
        "path(.a | length)",
    ] {
        assert!(
            matches!(drive_null(program, &mut resources), Err(EngineRunError::Raised(_))),
            "{program} must raise inside path mode"
        );
    }
    // The identity update still tracks: `path(reduce (10) as $v0 (.; .))`
    // publishes the empty path in both tools.
    assert!(matches!(
        drive_null("path(reduce (10) as $v0 (.; .))", &mut resources),
        Ok(1)
    ));
}

/// A `try` barrier over a GENERATOR body must not catch the SIBLING
/// operand's error. The barrier belongs to the operand that already ran, so
/// it is still on the stack with a live producer above it when the sibling
/// evaluates, and only the sibling-boundary raise clamp can keep it from
/// positionally enclosing the sibling's failure.
#[test]
fn a_generator_body_barrier_does_not_catch_the_binary_sibling() {
    let mut resources = resources();
    assert!(matches!(
        drive_null("error(\"b\") - (((1,2))?)", &mut resources),
        Err(EngineRunError::Raised(_))
    ));
}

/// A lazy consumer over a recursive callable truncates the recursion
/// itself: `limit(n; f)` pulls exactly `n` outputs, and the
/// countdown cut drops the nested machine driving `f`'s body at the moment
/// the count is spent. Without the cut the body materializes per
/// recursion level before its first output, so `[limit(2; nat)]` runs `nat`
/// to the depth ceiling and raises `recursive function calls exceeded the
/// depth ceiling of 512`. The n=1 case works either way (the first output
/// precedes the recursive branch); these pin the n>=2 cut.
#[test]
fn limit_truncates_a_recursive_callable() {
    let cases: [(&str, &[&str]); 3] = [
        ("def nat: 1, (nat|.+1); [limit(2; nat)]", &["1", "2"]),
        ("def nat: 1, (nat|.+1); [limit(3; nat)]", &["1", "2", "3"]),
        ("def f: 1, f; [limit(2; f)]", &["1", "1"]),
    ];
    for (program, expected) in cases {
        let mut resources = resources();
        let compiled = compiled_for_execute(program, &resources);
        let mut stream = match compiled
            .try_run_whole_value(CodecInputOutcome::Result(EngineResult::owned(Value::Null)), &resources)
            .expect("run seeds")
        {
            EngineRun::Stream { stream, .. } => stream,
            EngineRun::Suppressed => panic!("a limit over a generator is not suppressed"),
            EngineRun::Pushdown(error) => panic!("no pushdown over null: {error:?}"),
            EngineRun::ReboundWhole => panic!("whole-value run must not rebound"),
        };
        let mut arrays = Vec::new();
        loop {
            match stream.poll(&mut resources).expect("poll") {
                RunPoll::Item(item) => arrays.push(owned_array_integer_texts(&item)),
                RunPoll::Complete => break,
                RunPoll::Pending => panic!("credits are ample; no pause expected"),
            }
        }
        assert_eq!(
            arrays,
            [Vec::from(expected)],
            "{program} must publish the limited array instead of running to the depth ceiling"
        );
    }
}

/// The callable-body machine pool: the drive pools a
/// completed body machine instead of dropping it, including via the
/// completion-drain at route.rs — an Item whose machine has only drained
/// producers left drives one more advance so the pool fills BEFORE the
/// parent's next call admission, and a multi-output body's over-produced
/// item parks on the machine for the frame's next resumption. These rows
/// pin the observable contract of that machinery: multi-output bodies
/// publish in order, cuts still truncate between resumptions, and sibling
/// recursive calls answer byte-identically under reuse.
#[test]
fn pooled_callable_body_machines_keep_output_order_and_cuts() {
    let cases: [(&str, &[&str]); 5] = [
        // multi-output body: every extra item rides the one-deep queue
        ("def f: 1, 2; [f, f]", &["1", "2", "1", "2"]),
        // a cut between resumptions drops the queued item unpublished
        ("def f: 1, 2; [limit(1; f)]", &["1"]),
        ("def f: 1, 2; [limit(3; f)]", &["1", "2"]),
        // sibling non-tail calls reuse one machine per level
        (
            "def fib($n): if $n < 2 then $n else fib($n-1) + fib($n-2) end; fib(12)",
            &["144"],
        ),
        (
            "def fib: if . < 2 then . else (.-1 | fib) + (.-2 | fib) end; fib",
            &["55"],
        ),
    ];
    for (program, expected) in cases {
        let mut resources = resources();
        let compiled = compiled_for_execute(program, &resources);
        let input = if program.contains("fib: if") {
            CodecInputOutcome::Result(EngineResult::owned(int_value("10")))
        } else {
            CodecInputOutcome::Result(EngineResult::owned(Value::Null))
        };
        let mut stream = match compiled.try_run_whole_value(input, &resources).expect("run seeds") {
            EngineRun::Stream { stream, .. } => stream,
            EngineRun::Suppressed => panic!("a generator run is not suppressed: {program}"),
            EngineRun::Pushdown(error) => panic!("no pushdown expected: {error:?}"),
            EngineRun::ReboundWhole => panic!("whole-value run must not rebound"),
        };
        let mut texts = Vec::new();
        loop {
            match stream.poll(&mut resources).expect("poll") {
                RunPoll::Item(EngineResult::Owned(Value::Number(number))) => {
                    texts.push(String::from(number.to_integer().expect("integer").as_str()));
                }
                RunPoll::Item(item) => texts.extend(owned_array_integer_texts(&item)),
                RunPoll::Complete => break,
                RunPoll::Pending => panic!("credits are ample; no pause expected"),
            }
        }
        assert_eq!(
            texts,
            Vec::from(expected),
            "{program} must publish its outputs in order under body-machine pooling"
        );
    }
}

#[test]
fn limit_truncates_a_recursive_callable_past_the_depth_ceiling() {
    // The residual: a consumed output must not hold a recursion level
    // open, so `limit` over a recursive user def runs for n well past the
    // callable-depth ceiling instead of raising the depth-ceiling error.
    let cases: [(&str, &str); 3] = [
        // The comma-last-member shape: the self-call is the Choice's final
        // member, so each round trampolines instead of nesting.
        ("def f: 1, f; [limit(5000;f)]|length", "5000"),
        // The pipe-left shape: the self-call is the FlatMap's UPSTREAM,
        // whose continuation re-applies per output, so it trampolines too.
        ("def nat: 1, (nat|.+1); [limit(700;nat)]|length", "700"),
        // The pipe-tail shape inside a comma member (the re-verification
        // row, which the old marking never reached because the walk stopped
        // at the Choice).
        ("def f: ., ((.+1)|f); [limit(5000;f)]|length", "5000"),
    ];
    for (program, expected) in cases {
        // The crossing rows drive tens of thousands of work transitions
        // (one per resumption), far past one 4096-credit entry; the poll
        // loop below resumes fresh cooperative entries on each Pending.
        let mut resources = resources();
        let compiled = compiled_for_execute(program, &resources);
        let mut stream = match compiled
            .try_run_whole_value(CodecInputOutcome::Result(EngineResult::owned(Value::Null)), &resources)
            .expect("run seeds")
        {
            EngineRun::Stream { stream, .. } => stream,
            other => panic!("stream expected, got {other:?}"),
        };
        let mut texts = Vec::new();
        loop {
            match stream.poll(&mut resources).expect("poll") {
                RunPoll::Item(item) => {
                    let EngineResult::Owned(Value::Number(number)) = item else {
                        panic!("expected a number item, got {item:?}");
                    };
                    texts.push(String::from(number.to_integer().expect("integer").as_str()));
                }
                RunPoll::Complete => break,
                // The 4096-credit entry is a cooperative WINDOW, not a
                // run bound: resume with a fresh entry until completion.
                RunPoll::Pending => {
                    assert!(
                        resources.try_begin_next_cooperative_entry(4096).expect("resume"),
                        "a paused trampolined recursion must be resumable"
                    );
                }
            }
        }
        assert_eq!(texts, [expected], "{program} must answer past the old ceiling");
    }
}

/// A `limit`/`nth` cut must produce the same value and cardinality in every
/// consumer position, and must never panic. The leftover generator is
/// dropped without being evaluated (`limit(1; 1, error("x"))` is `1`).
#[test]
fn a_limit_or_nth_cut_is_safe_in_every_consumer_position() {
    let mut resources = resources();
    let null = Value::Null;
    let cases: &[(&str, &str)] = &[
        ("1 + limit(1;2,3)", "3\n"),
        ("[10,20,30] | map(.+limit(1;range(100)))", "[10,20,30]\n"),
        ("[10,20,30] | map(.+limit(1;range(1;100)))", "[11,21,31]\n"),
        ("{lo:5, xs:[1,2,3,4,5,6]} | .lo < limit(2;.xs[])", "false\nfalse\n"),
        ("[[1],[2,3]] | limit(2;.[]) < nth(1;.[])", "true\nfalse\n"),
        ("reduce limit(1;range(5)) as $x (0;99)", "99\n"),
        ("foreach limit(1;range(5)) as $x (0;.+1;.)", "1\n"),
        ("limit(1; 1, error(\"x\"))", "1\n"),
        ("try (1 + limit(1;2,3)) catch \"x\"", "3\n"),
        ("1 + limit(2;2,3)", "3\n4\n"),
        ("1 < limit(2;2,3)", "true\ntrue\n"),
        ("1 < limit(2;2,3,4)", "true\ntrue\n"),
    ];
    for (program, expected) in cases {
        let got = run_render(program, &null, &mut resources)
            .unwrap_or_else(|error| panic!("{program} must complete, got {error:?}"));
        assert_eq!(got, *expected, "{program}");
    }

    // Each truncating operand publishes exactly one value — `limit(1;2,3)`
    // the 2, `nth(1;2,3,4)` the 3 — so every spelling must publish the
    // operator's single answer for that value, never a panic and never a
    // second output. Slots per op: [1 op limit, limit op 1, 1 op nth,
    // nth op 1].
    let ops: &[(&str, [&str; 4])] = &[
        ("+", ["3\n", "3\n", "4\n", "4\n"]),
        ("-", ["-1\n", "1\n", "-2\n", "2\n"]),
        ("*", ["2\n", "2\n", "3\n", "3\n"]),
        ("/", ["0.5\n", "2\n", "0.3333333333333333\n", "3\n"]),
        ("%", ["1\n", "0\n", "1\n", "0\n"]),
        ("==", ["false\n", "false\n", "false\n", "false\n"]),
        ("!=", ["true\n", "true\n", "true\n", "true\n"]),
        ("<", ["true\n", "false\n", "true\n", "false\n"]),
        ("<=", ["true\n", "false\n", "true\n", "false\n"]),
        (">", ["false\n", "true\n", "false\n", "true\n"]),
        (">=", ["false\n", "true\n", "false\n", "true\n"]),
    ];
    for (op, expected) in ops {
        for (truncating, slot) in [("limit(1;2,3)", 0usize), ("nth(1;2,3,4)", 2usize)] {
            assert_eq!(
                run_render(&format!("1 {op} {truncating}"), &null, &mut resources)
                    .unwrap_or_else(|error| panic!("1 {op} {truncating} must complete, got {error:?}")),
                expected[slot],
                "1 {op} {truncating}"
            );
            assert_eq!(
                run_render(&format!("{truncating} {op} 1"), &null, &mut resources)
                    .unwrap_or_else(|error| panic!("{truncating} {op} 1 must complete, got {error:?}")),
                expected[slot + 1],
                "{truncating} {op} 1"
            );
        }
    }
    for truncating in ["limit(1;range(5))", "nth(1;range(5))"] {
        for program in [
            format!("reduce {truncating} as $x (0;99)"),
            format!("foreach {truncating} as $x (0;.+1;.)"),
        ] {
            run_render(&program, &null, &mut resources)
                .unwrap_or_else(|error| panic!("{program} must not panic: {error:?}"));
        }
    }

    let regression: &[(&str, &str)] = &[
        ("1 + first(2,3)", "3\n"),
        ("first(2,3) + 1", "3\n"),
        ("1 + last(2,3)", "4\n"),
        ("last(2,3) + 1", "4\n"),
        ("reduce first(range(5)) as $x (0;99)", "99\n"),
        ("reduce last(range(5)) as $x (0;99)", "99\n"),
        ("1 + (label $o | 2, break $o)", "3\n"),
        ("1 + while(false; .)", ""),
        ("1 + until(true; .)", "1\n"),
        ("1 + first(repeat(1))", "2\n"),
        ("reduce while(false; .) as $x (0;99)", "0\n"),
        ("reduce until(true; .) as $x (0;99)", "99\n"),
    ];
    for (program, expected) in regression {
        let got = run_render(program, &null, &mut resources)
            .unwrap_or_else(|error| panic!("{program} must complete, got {error:?}"));
        assert_eq!(got, *expected, "{program}");
    }
}

#[test]
fn a_tail_marked_call_answers_every_argument_combination() {
    // The tail trampoline must not drop the argument combinations beyond
    // the first: `f(4)` runs the body once per argument OUTPUT, and the
    // tail-marked self-call `f($a - 1, $a - 2)` must answer per
    // combination (`2 1 2`), never bind only the first combo (`2 2`).
    let program = "def f($a): if $a < 3 then $a else f($a - 1, $a - 2) end; f(4)";
    let mut resources = resources();
    let compiled = compiled_for_execute(program, &resources);
    let mut stream = match compiled
        .try_run_whole_value(CodecInputOutcome::Result(EngineResult::owned(Value::Null)), &resources)
        .expect("run seeds")
    {
        EngineRun::Stream { stream, .. } => stream,
        other => panic!("stream expected, got {other:?}"),
    };
    let mut texts = Vec::new();
    loop {
        match stream.poll(&mut resources).expect("poll") {
            RunPoll::Item(item) => {
                let EngineResult::Owned(Value::Number(number)) = item else {
                    panic!("expected a number item, got {item:?}");
                };
                texts.push(String::from(number.to_integer().expect("integer").as_str()));
            }
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("credits are ample; no pause expected"),
        }
    }
    assert_eq!(texts, ["2", "1", "2"]);
}

/// A chain of `depth` nested `{"a": …}` objects over the integer `1`.
fn nested_a(depth: usize, _resources: &ResourceContext<'_>) -> Value {
    let mut value = int_value("1");
    for _ in 0..depth {
        let mut builder = jqf_data::ObjectBuilder::try_with_capacity(1).expect("builder");
        builder
            .try_insert_last(jqf_data::ObjectKey::try_from_str("a").expect("key"), value)
            .expect("insert");
        value = Value::Object(builder.try_finish().expect("finish"));
    }
    value
}

/// Compiles `program` and renders every published item as one line of
/// JSON, stopping at the first raise.
fn run_render(program: &str, input: &Value, resources: &mut ResourceContext<'_>) -> Result<String, EngineRunError> {
    let compiled = compiled_for_execute(program, resources);
    let mut stream = match compiled
        .try_run_whole_value(CodecInputOutcome::Result(EngineResult::owned(input.clone())), resources)
        .expect("run seeds")
    {
        EngineRun::Stream { stream, .. } => stream,
        EngineRun::Suppressed => return Ok(String::from("<suppressed>")),
        EngineRun::Pushdown(error) => return Err(error),
        EngineRun::ReboundWhole => panic!("whole-value run must not rebound"),
    };
    let mut out = String::new();
    loop {
        match stream.poll(resources) {
            Ok(RunPoll::Item(EngineResult::Owned(value))) => {
                use core::fmt::Write;
                writeln!(out, "{}", jqf_builtins::semantics::render::to_json(&value)?).expect("write");
            }
            Ok(RunPoll::Item(EngineResult::Located(_))) => {
                return Err(super::internal_contract("path test must publish owned values"));
            }
            Ok(RunPoll::Complete) => return Ok(out),
            Ok(RunPoll::Pending) => {
                resources
                    .try_begin_next_cooperative_entry(4_096)
                    .expect("credits replenish");
            }
            Err(error) => return Err(error),
        }
    }
}

/// `path()` over a RECURSIVE user def must carry the register through the
/// call, because the call itself never touches the register: a def
/// body's navigations extend the caller's register and its emissions are
/// checked against it. The trampoline restart carries the register the
/// navigation before the tail call parked, and a returning call leaves the
/// register at the body's last state, so the caller's continuation sees
/// it; a nested per-call register would compare the leaf's emission
/// against a stale register and raise `Invalid path expression with
/// result 1` where the answer is `["a","a","a"]`.
#[test]
fn path_over_a_recursive_def_carries_the_register() {
    let mut resources = resources();
    let doc = nested_a(3, &resources);
    let recursive = "path(def f: if type == \"object\" then .a | f else . end; f)";
    assert_eq!(
        run_render(recursive, &doc, &mut resources).expect("run"),
        "[\"a\",\"a\",\"a\"]\n",
        "the recursive walk must publish the full path, not raise on the leaf"
    );
    assert_eq!(
        run_render(&format!("[.][0] | {recursive}"), &doc, &mut resources).expect("run"),
        "[\"a\",\"a\",\"a\"]\n",
        "the paired-route twin must answer identically"
    );
    // The register persists past a RETURNING call: the `.b` right after
    // the recursion navigates from the LEAF, so it fails the
    // `Cannot index number with string ("b")` refusal — not a
    // stale-register
    // admission rejection.
    assert!(matches!(
        run_render(&format!("{recursive} | .b"), &doc, &mut resources,),
        Err(EngineRunError::TypeMismatch { .. })
    ));
    // A NON-tail recursion publishes exactly the innermost emission: the
    // leaf's `.` is the one value that reaches the argument's sink, and
    // the register at that moment names the full path.
    let non_tail = "path(def f: if type == \"object\" then .a | (f | .) else . end; f)";
    assert_eq!(
        run_render(non_tail, &nested_a(2, &resources), &mut resources).expect("run"),
        "[\"a\",\"a\"]\n",
        "a non-tail recursion publishes the leaf once"
    );
    // The leaf's emission is published BEFORE the leaf's own raise, and
    // the raise must not lose the prefix.
    let emit_then_raise = "path(def f: if type == \"object\" then .a | f \
             else (., error(\"x\")) end; f)";
    let compiled = compiled_for_execute(emit_then_raise, &resources);
    let EngineRun::Stream { stream, .. } = compiled
        .try_run_whole_value(CodecInputOutcome::Result(EngineResult::owned(doc.clone())), &resources)
        .expect("run seeds")
    else {
        panic!("a path program is a stream")
    };
    let mut stream = stream;
    let mut prefix = String::new();
    let mut failure = None;
    loop {
        match stream.poll(&mut resources) {
            Ok(RunPoll::Item(EngineResult::Owned(value))) => {
                use core::fmt::Write;
                writeln!(
                    prefix,
                    "{}",
                    jqf_builtins::semantics::render::to_json(&value).expect("render")
                )
                .expect("write");
            }
            Ok(RunPoll::Complete) => break,
            Ok(RunPoll::Pending) => panic!("ample credits; no pause expected"),
            Err(error) => {
                failure = Some(error);
                break;
            }
            Ok(RunPoll::Item(EngineResult::Located(_))) => {
                panic!("path test must publish owned values")
            }
        }
    }
    assert_eq!(
        prefix, "[\"a\",\"a\",\"a\"]\n",
        "the leaf's emission must be published before the raise"
    );
    assert!(
        matches!(failure, Some(EngineRunError::Raised(_))),
        "the leaf's raise must surface after the published prefix"
    );
}

/// perf/number-inline-float — the inline-float arm's `identical` law: a
/// navigation-extracted float is the register's value by BITS, `NaN`
/// included, while a computed or literal float is a fresh object and is
/// rejected. The `==` form of the comparison answered `false` for a
/// navigation-extracted `NaN` (the bits are equal, the VALUES are not), so
/// `path(.[0])` over `[NaN]` raised `Invalid path expression with result
/// null` where the answer is `[0]`, and the `.[] = 1` row over the
/// non-finite array died on its `NaN` element. The law is bit equality
/// (`to_bits()`).
#[test]
fn pathmode_nan_navigation_keeps_the_register_value() {
    let mut resources = resources();
    let doc = {
        let mut array = jqf_data::Array::try_with_capacity(6).expect("array");
        for value in [
            Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(1))),
            Value::Null,
            Value::Number(jqf_data::Number::float(jqf_data::Float::new(f64::INFINITY))),
            Value::Number(jqf_data::Number::float(jqf_data::Float::new(f64::NEG_INFINITY))),
            Value::Number(jqf_data::Number::float(jqf_data::Float::new(f64::NAN))),
            Value::Number(jqf_data::Number::float(jqf_data::Float::new(-f64::NAN))),
        ] {
            array.try_push(value).expect("push");
        }
        Value::Array(array)
    };
    // A navigation-extracted NaN keeps the register value: `path(.[0])`
    // over `[NaN]` publishes `[0]`, exactly as the allocation-identity
    // test answers for the same boxed bits.
    assert_eq!(
        run_render("path(.[0])", &doc, &mut resources).expect("run"),
        "[0]\n",
        "navigation-extracted NaN is the register's value"
    );
    // The row that exposed the `==` law over non-finite elements.
    assert_eq!(
        run_render(".[] = 1", &doc, &mut resources).expect("run"),
        "[1,1,1,1,1,1]\n",
        "update assignment over non-finite elements must publish"
    );
    // A LITERAL float is a fresh object: `path(1.5)` over `1.5` rejects,
    // exactly as `path(7)` over `7` rejects for the machine arm.
    let literal = Value::Number(jqf_data::Number::float(jqf_data::Float::new(1.5)));
    assert!(matches!(
        run_render("path(1.5)", &literal, &mut resources),
        Err(EngineRunError::Raised(_))
    ));
}

/// A deep tail-recursive def inside `path(…)` must hit the
/// callable-depth ceiling instead of dying by native stack overflow:
/// the tail body recurses on the native Rust stack, and `call_def` bounds
/// that path with the same [`CALLABLE_DEPTH_LIMIT`] law as the non-tail
/// branch. The deep doc and the drop of it recurse, so the whole body runs
/// on a 64 MiB thread, exactly as the decode nesting tests do.
#[test]
fn a_tail_recursive_def_inside_path_hits_the_callable_depth_ceiling() {
    extern crate std;
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(|| {
            let mut resources = resources();
            // The machine's TAIL trampoline is FLAT: a document-driven
            // tail recursion — `f` recurses once per nesting level — does
            // not recurse on native stack, so a depth far past any
            // recursion ceiling completes, publishing the full path. The
            // bounded twin pins the shape: the register accumulates the
            // `"a"` steps across the trampoline rounds.
            let deep = "path(def f: if type == \"object\" then .a | f else . end; f)";
            let doc = nested_a(5000, &resources);
            let path = run_render(deep, &doc, &mut resources).expect("deep tail recursion completes");
            assert_eq!(path.matches('"').count(), 5000 * 2, "one \"a\" step per level");
        })
        .expect("big-stack thread spawns")
        .join()
        .expect("big-stack thread does not panic");
}

#[test]
fn a_generator_body_barrier_does_not_catch_the_and_sibling() {
    let mut resources = resources();
    assert!(matches!(
        drive_null("((1,2))? and error(\"b\")", &mut resources),
        Err(EngineRunError::Raised(_))
    ));
}

#[test]
fn a_generator_body_barrier_does_not_catch_a_sibling_object_member() {
    let mut resources = resources();
    assert!(matches!(
        drive_null("{a: ((1,2))?, b: error(\"x\")}", &mut resources),
        Err(EngineRunError::Raised(_))
    ));
}

#[test]
fn a_generator_body_barrier_does_not_catch_a_sibling_reduce_update() {
    let mut resources = resources();
    assert!(matches!(
        drive_null("reduce (((1,2))?) as $x (0; error(\"b\"))", &mut resources),
        Err(EngineRunError::Raised(_))
    ));
}

#[test]
fn a_generator_body_barrier_does_not_catch_a_sibling_foreach_update() {
    let mut resources = resources();
    assert!(matches!(
        drive_null("foreach (((1,2))?) as $x (0; error(\"b\"))", &mut resources),
        Err(EngineRunError::Raised(_))
    ));
}

#[test]
fn a_generator_body_barrier_does_not_catch_a_sibling_type_error() {
    // `map(.)` on null raises the iterate class; the `?` on the already-run
    // right operand must not swallow it. The two classes below are the same
    // typed vocabulary the mid-residual failure uses.
    let mut resources = resources();
    assert!(matches!(
        drive_null("map(.) + (((1,2))?)", &mut resources),
        Err(EngineRunError::IterateMismatch { .. } | EngineRunError::TypeMismatch { .. })
    ));
}

#[test]
fn a_path_mode_argument_barrier_does_not_catch_a_downstream_step() {
    // The same family through PATH MODE: a path-family or regex builtin
    // evaluates its argument filter in the eager owned run, and there the
    // `?`'s own sink wrapper must let a DOWNSTREAM step of the try's output
    // (the `[0]` index, the `[-2:-2]` slice) escape — `(E)?[i]` raises, it
    // does not swallow.
    let mut resources = resources();
    assert!(
        drive_null("getpath(({x:1}?)[0])", &mut resources).is_err(),
        "getpath(({{x:1}}?)[0]) must raise, not swallow"
    );
    assert!(
        drive_null("scan(((({x:1})?)[-2:-2]); \"gims\")", &mut resources).is_err(),
        "scan(({{x:1}}?)[-2:-2]) must raise, not swallow"
    );
}

#[test]
fn a_path_mode_argument_barrier_still_catches_its_own_body() {
    // The counterweight in path mode: the `?` still swallows an error in
    // its OWN body. `getpath([(1, error("x"))?])` answers `null`
    // (the argument is `[1]`, the error caught), and a swallowing `?` is
    // exactly what produces that; a `?` that let the error out would raise.
    let mut resources = resources();
    assert!(
        matches!(drive_null("getpath([(1, error(\"x\"))?])", &mut resources), Ok(1)),
        "path-mode `?` still swallows its own body's error"
    );
}

/// The counterweight: the same family, but the sibling's OWN barrier or an
/// ENCLOSING barrier still catches, and a sibling's own `try` still answers.
/// These pin that the raise clamp only skips stale barriers below the
/// sibling-start frame, never the sibling's own or the enclosing ones.
#[test]
fn an_enclosing_try_still_catches_and_a_siblings_own_barrier_still_answers() {
    let mut resources = resources();
    let got = drive_null_values("try (error(\"a\") - ((1,2))?)", &mut resources).expect("outer try");
    assert!(
        got.is_empty(),
        "the outer try still swallows the binary's error, got {got:?}"
    );
    assert_eq!(
        drive_null_values("(try error(\"d\") catch 9) - ((1,2))?", &mut resources).expect("sibling try"),
        ["8", "7"],
        "the sibling's own try answers once per right-hand value"
    );
    let got = drive_null_values("try {a: ((1,2))?, b: error(\"x\")}", &mut resources).expect("object try");
    assert!(
        got.is_empty(),
        "the outer try still swallows the object's error, got {got:?}"
    );
    assert_eq!(
        drive_null_values("{a: ((1,2))?, b: (try error(\"x\") catch 5)}", &mut resources).expect("member try"),
        ["{\"a\":1,\"b\":5}", "{\"a\":2,\"b\":5}"],
        "a member's own try still answers both combinations"
    );
    let got = drive_null_values("try reduce (((1,2))?) as $x (0; error(\"b\"))", &mut resources).expect("fold try");
    assert!(
        got.is_empty(),
        "the outer try still swallows the fold's error, got {got:?}"
    );
    assert_eq!(
        drive_null_values("((try error(\"c\") catch 5) + 1) - ((1,2))?", &mut resources).expect("nested binary"),
        ["5", "4"],
        "a nested binary's inner try still answers both combinations"
    );
}

#[test]
fn choice_emits_the_complete_left_then_the_complete_right() {
    // `.a, .b` over `{"a":1,"b":2}`: a `Choice` evaluates the left member
    // fully, then the right member over the same input.
    let mut resources = resources();
    let input = located_value(
        &TestValue::Obj(&[("a", TestValue::Int("1")), ("b", TestValue::Int("2"))]),
        &resources,
    );
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("a", false)],
        },
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("b", false)],
        },
        ProgramNode::Choice {
            left: nid(0),
            right: nid(1),
        },
    ];
    assert_eq!(drive_ints(&nodes, nid(2), input, &mut resources), ["1", "2"]);
}

#[test]
fn choice_fork_reborrows_a_located_input_and_clones_owned_null() {
    // The fork re-derives a second owner over the SAME input. A located value
    // re-borrows the same node (fallible seam, here Ok); an owned null clones.
    let resources = resources();
    let located = located_value(&TestValue::Int("7"), &resources);
    let EngineResult::Located(original) = &located else {
        panic!("located input");
    };
    let reborrow = located.try_clone().expect("located re-borrow");
    let EngineResult::Located(cloned) = &reborrow else {
        panic!("re-borrow stays located");
    };
    assert_eq!(
        original.node(),
        cloned.node(),
        "the fork re-borrows the same authoritative node"
    );

    let owned = EngineResult::owned(Value::Null);
    assert!(matches!(
        owned.try_clone().expect("owned null clones"),
        EngineResult::Owned(Value::Null)
    ));
}

#[test]
fn left_member_error_tears_down_while_the_right_is_pending() {
    // `.a.x, .b` over `{"a":5,"b":2}`: the left member indexes into the number
    // `5` and aborts; the pending right member never runs and the whole run is
    // torn down — error exit 5, nothing emitted.
    let mut resources = resources();
    let input = located_value(
        &TestValue::Obj(&[("a", TestValue::Int("5")), ("b", TestValue::Int("2"))]),
        &resources,
    );
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("a", false), key("x", false)],
        },
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("b", false)],
        },
        ProgramNode::Choice {
            left: nid(0),
            right: nid(1),
        },
    ];
    let mut stream =
        EngineRunStream::seed(&nodes, nid(2), nid(2), 0, 0, &[], &[], &[], input, None).expect("test seed");
    assert!(matches!(
        stream.poll(&mut resources).expect_err("left member aborts"),
        EngineRunError::TypeMismatch { .. }
    ));
    // The torn-down run yields no further items: the right member is gone.
    assert!(matches!(
        stream.poll(&mut resources).expect("post-teardown"),
        RunPoll::Complete
    ));
}

#[test]
fn cooperative_pause_resumes_inside_a_choice_preserving_order() {
    // Drive `.a, .b` with a one-credit window: every resume must continue the
    // Choice exactly, emitting both members in order.
    let mut resources = resources_with_credits(1);
    assert!(matches!(
        resources.admit_work_transition().expect("prime"),
        jqf_resource::WorkAdmission::Granted(_)
    ));
    let input = located_value(
        &TestValue::Obj(&[("a", TestValue::Int("1")), ("b", TestValue::Int("2"))]),
        &resources,
    );
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("a", false)],
        },
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("b", false)],
        },
        ProgramNode::Choice {
            left: nid(0),
            right: nid(1),
        },
    ];
    let mut stream =
        EngineRunStream::seed(&nodes, nid(2), nid(2), 0, 0, &[], &[], &[], input, None).expect("test seed");
    let mut emitted = Vec::new();
    let mut guard = 0;
    loop {
        guard += 1;
        assert!(guard < 100, "the paused choice must make progress");
        match stream.poll(&mut resources).expect("poll") {
            RunPoll::Pending => {
                assert!(resources.try_begin_next_cooperative_entry(1).expect("resume"));
            }
            RunPoll::Item(item) => emitted.push(integer_of(&item)),
            RunPoll::Complete => break,
        }
    }
    assert_eq!(emitted, ["1", "2"]);
}

#[test]
fn flatmap_over_a_choice_runs_the_body_per_member() {
    // The named FlatMap-arm producer `(.a, .b).c` = FlatMap(Choice, Stage[c]):
    // over `{"a":{"c":1},"b":{"c":2}}` the body `.c` runs once per Choice
    // member, emitting `1` then `2`.
    let mut resources = resources();
    let input = located_value(
        &TestValue::Obj(&[
            ("a", TestValue::Obj(&[("c", TestValue::Int("1"))])),
            ("b", TestValue::Obj(&[("c", TestValue::Int("2"))])),
        ]),
        &resources,
    );
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("a", false)],
        },
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("b", false)],
        },
        ProgramNode::Choice {
            left: nid(0),
            right: nid(1),
        },
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("c", false)],
        },
        ProgramNode::FlatMap {
            upstream: nid(2),
            body: nid(3),
        },
    ];
    assert_eq!(drive_ints(&nodes, nid(4), input, &mut resources), ["1", "2"]);
}

#[test]
fn flatmap_body_suppression_skips_a_member_and_continues_the_choice() {
    // The pinned emission-routing probe: `(.a, .b).c?` over
    // `{"a":5,"b":{"c":2}}` → `2`. The body `.c?` suppresses member a's number
    // (a skip, no output), and the UPSTREAM Choice continues to member b.
    let mut resources = resources();
    let input = located_value(
        &TestValue::Obj(&[
            ("a", TestValue::Int("5")),
            ("b", TestValue::Obj(&[("c", TestValue::Int("2"))])),
        ]),
        &resources,
    );
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("a", false)],
        },
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("b", false)],
        },
        ProgramNode::Choice {
            left: nid(0),
            right: nid(1),
        },
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("c", true)],
        },
        ProgramNode::FlatMap {
            upstream: nid(2),
            body: nid(3),
        },
    ];
    assert_eq!(drive_ints(&nodes, nid(4), input, &mut resources), ["2"]);
}

#[test]
fn emission_routes_through_nested_flatmap_bodies() {
    // `.[] | (.a, .b).c` = FlatMap(Stage[Each], FlatMap(Choice, Stage[c])):
    // over `[{"a":{"c":1},"b":{"c":2}}]` the inner body's `.c` outputs must
    // route PAST BOTH the inner and outer FlatMapBody frames to the top-level
    // stream — `1` then `2`, not re-consumed.
    let mut resources = resources();
    let element = TestValue::Obj(&[
        ("a", TestValue::Obj(&[("c", TestValue::Int("1"))])),
        ("b", TestValue::Obj(&[("c", TestValue::Int("2"))])),
    ]);
    let input = located_array_of(&[element], &resources);
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![each(false)],
        }, // 0: .[]
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("a", false)],
        }, // 1
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("b", false)],
        }, // 2
        ProgramNode::Choice {
            left: nid(1),
            right: nid(2),
        }, // 3
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("c", false)],
        }, // 4
        ProgramNode::FlatMap {
            upstream: nid(3),
            body: nid(4),
        }, // 5: (.a,.b).c
        ProgramNode::FlatMap {
            upstream: nid(0),
            body: nid(5),
        }, // 6: .[] | (.a,.b).c
    ];
    assert_eq!(drive_ints(&nodes, nid(6), input, &mut resources), ["1", "2"]);
}

// --- Constructors + literals (barrier, owned navigation, key mismatch) ------

/// Drives a graph stream to completion, collecting each emitted OWNED value.
fn drive_owned(
    nodes: &[ProgramNode],
    root: ProgramNodeId,
    input: EngineResult<'_>,
    resources: &mut ResourceContext<'_>,
) -> Vec<Value> {
    let mut stream = EngineRunStream::seed(nodes, root, root, 0, 0, &[], &[], &[], input, None).expect("test seed");
    let mut emitted = Vec::new();
    loop {
        match stream.poll(resources).expect("poll") {
            RunPoll::Item(item) => match item {
                EngineResult::Owned(value) => emitted.push(value),
                EngineResult::Located(_) => panic!("a construction barrier emits owned values"),
            },
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("ample credits; no pause expected"),
        }
    }
    emitted
}

/// The `a`/`b` integer members of an owned object, as `(a, b)` text.
fn object_ab(value: &Value) -> (String, String) {
    let Value::Object(object) = value else {
        panic!("expected an owned object, got {value:?}");
    };
    let member = |key: &str| match object.get(key) {
        Some(Value::Number(number)) => String::from(number.to_integer().expect("int").as_str()),
        other => panic!("member {key} not an integer: {other:?}"),
    };
    (member("a"), member("b"))
}

#[test]
fn literal_start_stage_emits_the_owned_literal_ignoring_input() {
    // A bare literal-start stage `7`: it ignores the input and emits the owned
    // number, then completes.
    let mut resources = resources();
    let input = located_bool(&resources);
    let nodes = vec![ProgramNode::Stage {
        start: StageStart::Literal(Value::Number(jqf_data::Number::try_json_literal("7").expect("literal"))),
        steps: Vec::new(),
    }];
    let emitted = drive_owned(&nodes, ROOT, input, &mut resources);
    assert_eq!(emitted.len(), 1);
    let Value::Number(number) = &emitted[0] else {
        panic!("expected an owned number");
    };
    assert_eq!(number.to_integer().as_ref().map(jqf_data::Integer::as_str), Some("7"));
}

#[test]
fn collect_array_emits_one_owned_array_of_the_fan_out() {
    // `[.[]]` over `[10,20,30]` collects the fan-out into ONE owned array — the
    // construction barrier materializes each located child at capture.
    let mut resources = resources();
    let input = located_int_array(&resources);
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![each(false)],
        },
        ProgramNode::CollectArray { body: Some(nid(0)) },
    ];
    let emitted = drive_owned(&nodes, nid(1), input, &mut resources);
    assert_eq!(emitted.len(), 1, "collect emits exactly one array");
    let Value::Array(array) = &emitted[0] else {
        panic!("expected an owned array");
    };
    assert_eq!(array.len(), 3);
}

#[test]
fn a_collect_is_metered_per_frame_task_and_pauses_mid_drive() {
    // The sampling law: the graph machine's advance loop admits one
    // work transition per frame task, so a COLLECTING drive — which emits
    // exactly one item — pauses mid-drive when the slice runs out instead
    // of running its whole body under a single admission. This is the
    // mechanism the RSS governor's overshoot bound rests on: without it, a
    // runaway collect could never be refused until it completed. Under a
    // one-credit window the collect must pause several times and still
    // emit the complete array in order.
    let mut resources = resources_with_credits(1);
    assert!(matches!(
        resources.admit_work_transition().expect("prime"),
        jqf_resource::WorkAdmission::Granted(_)
    ));
    let input = located_int_array(&resources);
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![each(false)],
        },
        ProgramNode::CollectArray { body: Some(nid(0)) },
    ];
    let mut stream =
        EngineRunStream::seed(&nodes, nid(1), nid(1), 0, 0, &[], &[], &[], input, None).expect("test seed");
    let mut emitted = Vec::new();
    let mut pauses = 0;
    let mut guard = 0;
    loop {
        guard += 1;
        assert!(guard < 200, "the paused collect must make progress");
        match stream.poll(&mut resources).expect("poll") {
            RunPoll::Pending => {
                pauses += 1;
                assert!(resources.try_begin_next_cooperative_entry(1).expect("resume"));
            }
            RunPoll::Item(EngineResult::Owned(value)) => emitted.push(value),
            RunPoll::Item(EngineResult::Located(_)) => {
                panic!("a collect's barrier emits owned values")
            }
            RunPoll::Complete => break,
        }
    }
    assert!(
        pauses >= 2,
        "a three-element collect must pause at least twice under a one-credit window"
    );
    assert_eq!(emitted.len(), 1, "collect still emits exactly one array");
    let Value::Array(array) = &emitted[0] else {
        panic!("expected an owned array");
    };
    assert_eq!(array.len(), 3, "the array survives the pauses complete");
}

#[test]
fn empty_body_collect_emits_the_empty_array() {
    // `[]` ignores its input and emits one empty owned array.
    let mut resources = resources();
    let input = located_bool(&resources);
    let nodes = vec![ProgramNode::CollectArray { body: None }];
    let emitted = drive_owned(&nodes, ROOT, input, &mut resources);
    assert_eq!(emitted.len(), 1);
    assert!(matches!(&emitted[0], Value::Array(array) if array.is_empty()));
}

#[test]
fn object_cartesian_later_member_varies_fastest() {
    // `{a: .l[], b: .m[]}` over `{"l":[1,2],"m":[3,4]}` → the depth-first
    // Cartesian product with the LATER member (b) varying fastest.
    let mut resources = resources();
    let input = located_value(
        &TestValue::Obj(&[
            ("l", TestValue::Arr(&[TestValue::Int("1"), TestValue::Int("2")])),
            ("m", TestValue::Arr(&[TestValue::Int("3"), TestValue::Int("4")])),
        ]),
        &resources,
    );
    // Static keys `a`/`b` lower to singleton literal-string producers.
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Literal(Value::try_string("a").expect("literal string")),
            steps: Vec::new(),
        }, // 0 key a
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("l", false), each(false)],
        }, // 1 value .l[]
        ProgramNode::Stage {
            start: StageStart::Literal(Value::try_string("b").expect("literal string")),
            steps: Vec::new(),
        }, // 2 key b
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("m", false), each(false)],
        }, // 3 value .m[]
        ProgramNode::ConstructObject {
            members: vec![
                ObjectMemberNode {
                    key: nid(0),
                    value: nid(1),
                    static_key: None,
                },
                ObjectMemberNode {
                    key: nid(2),
                    value: nid(3),
                    static_key: None,
                },
            ],
        }, // 4
    ];
    let emitted = drive_owned(&nodes, nid(4), input, &mut resources);
    let pairs: Vec<(String, String)> = emitted.iter().map(object_ab).collect();
    assert_eq!(
        pairs,
        [
            (String::from("1"), String::from("3")),
            (String::from("1"), String::from("4")),
            (String::from("2"), String::from("3")),
            (String::from("2"), String::from("4")),
        ]
    );
}

#[test]
fn non_string_dynamic_key_is_the_object_key_mismatch_class() {
    // `{(.n): .x}` over `{"n":5,"x":1}`: the dynamic key is a number — the
    // THIRD mismatch class, distinct from index/iterate, discovered
    // mid-construction and surfaced as a poll error.
    let mut resources = resources();
    let input = located_value(
        &TestValue::Obj(&[("n", TestValue::Int("5")), ("x", TestValue::Int("1"))]),
        &resources,
    );
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("n", false)],
        },
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("x", false)],
        },
        ProgramNode::ConstructObject {
            members: vec![ObjectMemberNode {
                key: nid(0),
                value: nid(1),
                static_key: None,
            }],
        },
    ];
    let mut stream =
        EngineRunStream::seed(&nodes, nid(2), nid(2), 0, 0, &[], &[], &[], input, None).expect("test seed");
    assert!(matches!(
        stream
            .poll(&mut resources)
            .expect_err("a non-string dynamic key aborts"),
        EngineRunError::ObjectKeyMismatch {
            actual_type: ValueKind::Number,
            ..
        }
    ));
}

#[test]
fn owned_array_index_navigates_a_constructor_result() {
    // `[.[]][0]` over `[10,20,30]`: the collected owned array is indexed,
    // cloning the first child out — the owned-container navigation arm.
    let mut resources = resources();
    let input = located_int_array(&resources);
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![each(false)],
        }, // 0 .[]
        ProgramNode::CollectArray { body: Some(nid(0)) }, // 1 [.[]]
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![StageStep::new(StepAccess::Index(0), false)],
        }, // 2 [0]
        ProgramNode::FlatMap {
            upstream: nid(1),
            body: nid(2),
        }, // 3 [.[]][0]
    ];
    let emitted = drive_owned(&nodes, nid(3), input, &mut resources);
    assert_eq!(emitted.len(), 1);
    let Value::Number(number) = &emitted[0] else {
        panic!("expected the first element");
    };
    assert_eq!(number.to_integer().as_ref().map(jqf_data::Integer::as_str), Some("10"));
}

/// Builds a located array whose single element is `elements[0]`, over an
/// object/int [`TestValue`] spec — for the nested-fan-out routing test.
fn located_array_of(elements: &[TestValue], resources: &ResourceContext<'_>) -> EngineResult<'static> {
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder");
    let items: Vec<NodeId> = elements
        .iter()
        .map(|value| build_value(&mut builder, value, resources))
        .collect();
    let root = builder
        .add_node(
            "test.array",
            AccountedSemanticNode::Array { item_role: "test.item" },
            None,
            resources,
        )
        .expect("array");
    for item in items {
        builder
            .add_occurrence(LocalOwnerRef::Node(root), "test.item", None, item, resources)
            .expect("item edge");
    }
    let document = builder.finish(root, resources).expect("document");
    let product = DocumentProduct::try_new(document, resources).expect("product");
    let handle = product.document().root_handle();
    EngineResult::Located(LocatedProduct::try_new(&product, handle).expect("located"))
}

/// Builds a `Missing` outcome carrying real located authority.
fn missing_outcome(resources: &ResourceContext<'_>) -> CodecInputOutcome<'static> {
    let (outcome, _report) = missing_result(resources).into_parts();
    outcome
}

fn missing_authority(resources: &ResourceContext<'_>) -> LocatedOutcome<'static> {
    match missing_result(resources).into_parts().0 {
        CodecInputOutcome::Missing { authority } => authority,
        _ => unreachable!("missing_result builds a Missing outcome"),
    }
}

fn missing_result(resources: &ResourceContext<'_>) -> CodecInputResult<'static> {
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder");
    let root = builder
        .add_node("test.bool", AccountedSemanticNode::Bool(true), None, resources)
        .expect("root");
    let product =
        DocumentProduct::try_new(builder.finish(root, resources).expect("document"), resources).expect("product");
    let located = LocatedOutcome::try_new(
        &product,
        ExactSelectionRecord::Missing {
            step_index: 0,
            origin: SelectionOrigin::new(0),
        },
    )
    .expect("located");
    CodecInputResult::try_from_access(AccessResult::from_outcome(AccessOutcome::Located(located))).expect("handoff")
}

// --- Control-flow families (Conditional / Alternative / Logical) -----------

/// A literal-start stage producing an owned scalar number, ignoring its input.
fn lit_num(text: &str) -> ProgramNode {
    ProgramNode::Stage {
        start: StageStart::Literal(Value::Number(
            jqf_data::Number::try_json_literal(text).expect("number literal"),
        )),
        steps: Vec::new(),
    }
}

/// A literal-start stage producing an owned boolean, ignoring its input.
fn lit_bool(value: bool) -> ProgramNode {
    ProgramNode::Stage {
        start: StageStart::Literal(Value::Bool(value)),
        steps: Vec::new(),
    }
}

/// A literal-start stage producing owned `null`, ignoring its input.
fn lit_null() -> ProgramNode {
    ProgramNode::Stage {
        start: StageStart::Literal(Value::Null),
        steps: Vec::new(),
    }
}

/// A path stage `.x.y` — two field steps that error over a non-object input.
fn path_xy() -> ProgramNode {
    ProgramNode::Stage {
        start: StageStart::Current,
        steps: vec![key("x", false), key("y", false)],
    }
}

/// The bool children of an owned `Value::Array`, for the discriminator witness.
fn array_bools(value: &Value) -> Vec<bool> {
    let Value::Array(array) = value else {
        panic!("expected an owned array, got {value:?}");
    };
    let mut out = Vec::new();
    let mut index = 0;
    while let Some(child) = array.get(index) {
        match child {
            Value::Bool(value) => out.push(*value),
            other => panic!("expected a bool child, got {other:?}"),
        }
        index += 1;
    }
    out
}

/// Renders an owned scalar (bool/number/null) as a compact token for order
/// assertions across a paused-and-resumed run.
fn owned_token(value: &Value) -> String {
    match value {
        Value::Bool(true) => String::from("true"),
        Value::Bool(false) => String::from("false"),
        Value::Null => String::from("null"),
        Value::Number(number) => String::from(number.to_integer().expect("int").as_str()),
        other => panic!("unexpected owned value {other:?}"),
    }
}

/// Drives a graph stream to completion under a ONE-credit window, forcing a
/// cooperative pause between advances; collects each emitted owned value's
/// token. Pins that a frame family resumes its continuation exactly.
fn drive_owned_paused(nodes: &[ProgramNode], root: ProgramNodeId, input: EngineResult<'_>) -> Vec<String> {
    let mut resources = resources_with_credits(1);
    assert!(matches!(
        resources.admit_work_transition().expect("prime"),
        jqf_resource::WorkAdmission::Granted(_)
    ));
    let mut stream = EngineRunStream::seed(nodes, root, root, 0, 0, &[], &[], &[], input, None).expect("test seed");
    let mut emitted = Vec::new();
    let mut guard = 0;
    loop {
        guard += 1;
        assert!(guard < 200, "the paused run must make progress");
        match stream.poll(&mut resources).expect("poll") {
            RunPoll::Pending => {
                assert!(resources.try_begin_next_cooperative_entry(1).expect("resume"));
            }
            RunPoll::Item(EngineResult::Owned(value)) => emitted.push(owned_token(&value)),
            RunPoll::Item(EngineResult::Located(located)) => {
                emitted.push(integer_of(&EngineResult::Located(located)));
            }
            RunPoll::Complete => break,
        }
    }
    emitted
}

#[test]
fn logical_and_is_left_outer_the_discriminator() {
    // `[(1,null) and (2,null)]` → `[true,false,false]` (3 outputs). LEFT is the
    // outer loop: left=1 (truthy, `and`) expands over the right (2→true,
    // null→false); left=null (falsy) short-circuits to one `false`. A
    // right-outer drive would give four outputs — this pins the orientation.
    let mut resources = resources();
    let input = located_bool(&resources);
    let nodes = vec![
        lit_num("1"),
        lit_null(),
        ProgramNode::Choice {
            left: nid(0),
            right: nid(1),
        }, // 2: (1,null)
        lit_num("2"),
        lit_null(),
        ProgramNode::Choice {
            left: nid(3),
            right: nid(4),
        }, // 5: (2,null)
        ProgramNode::Logical {
            operator: LogicalOp::And,
            left: nid(2),
            right: nid(5),
        }, // 6
        ProgramNode::CollectArray { body: Some(nid(6)) }, // 7
    ];
    let emitted = drive_owned(&nodes, nid(7), input, &mut resources);
    assert_eq!(emitted.len(), 1, "one collected array");
    assert_eq!(array_bools(&emitted[0]), [true, false, false]);
}

#[test]
fn logical_and_short_circuits_without_evaluating_a_would_error_right() {
    // `false and .x.y` over `5`: `and`+falsy short-circuits to one `false` and
    // NEVER evaluates the right, so the `.x.y` path that would error over the
    // number `5` is not touched — no error, one owned `false`.
    let mut resources = resources();
    let input = located_value(&TestValue::Int("5"), &resources);
    let nodes = vec![
        lit_bool(false),
        path_xy(),
        ProgramNode::Logical {
            operator: LogicalOp::And,
            left: nid(0),
            right: nid(1),
        },
    ];
    let mut stream =
        EngineRunStream::seed(&nodes, nid(2), nid(2), 0, 0, &[], &[], &[], input, None).expect("test seed");
    match stream
        .poll(&mut resources)
        .expect("short-circuit never evaluates the would-error right")
    {
        RunPoll::Item(EngineResult::Owned(Value::Bool(value))) => assert!(!value),
        other => panic!("expected one owned false, got {other:?}"),
    }
    assert!(matches!(
        stream.poll(&mut resources).expect("completion"),
        RunPoll::Complete
    ));
}

#[test]
fn alternative_seeds_the_right_on_left_complete_with_zero_truthy() {
    // `(false, null) // 9` → `9`: every left output is falsy (swallowed), so on
    // left-complete-with-zero-truthy the right fallback is seeded exactly once.
    let mut resources = resources();
    let input = located_bool(&resources);
    let nodes = vec![
        lit_bool(false),
        lit_null(),
        ProgramNode::Choice {
            left: nid(0),
            right: nid(1),
        }, // 2: (false,null)
        lit_num("9"),
        ProgramNode::Alternative {
            left: nid(2),
            right: nid(3),
        }, // 4
    ];
    let emitted = drive_owned(&nodes, nid(4), input, &mut resources);
    assert_eq!(emitted.len(), 1);
    assert_eq!(owned_token(&emitted[0]), "9");
}

#[test]
fn alternative_left_error_after_only_falsy_beats_the_pending_right() {
    // `(false, .x.y) // 9` over `5`: the left emits one falsy output (swallowed)
    // then the `.x.y` path errors over the number `5`. The error BEATS the
    // pending right — the fallback `9` is never seeded, and nothing is emitted.
    let mut resources = resources();
    let input = located_value(&TestValue::Int("5"), &resources);
    let nodes = vec![
        lit_bool(false),
        path_xy(),
        ProgramNode::Choice {
            left: nid(0),
            right: nid(1),
        }, // 2: (false, .x.y)
        lit_num("9"),
        ProgramNode::Alternative {
            left: nid(2),
            right: nid(3),
        }, // 4
    ];
    let mut stream =
        EngineRunStream::seed(&nodes, nid(4), nid(4), 0, 0, &[], &[], &[], input, None).expect("test seed");
    assert!(matches!(
        stream
            .poll(&mut resources)
            .expect_err("the left error beats the pending right"),
        EngineRunError::TypeMismatch { .. }
    ));
    // Torn down: the fallback `9` never surfaces.
    assert!(matches!(
        stream.poll(&mut resources).expect("post-teardown"),
        RunPoll::Complete
    ));
}

#[test]
fn alternative_truthy_passes_through_to_a_collect_above() {
    // `[.a // .b]` over `{"a":5,"b":7}` → `[5]`: the truthy left output `.a`
    // (5) is re-emitted upward as a FILTERING PASS-THROUGH straight into the
    // enclosing `Collect` consumer, and the right `.b` is never seeded.
    let mut resources = resources();
    let input = located_value(
        &TestValue::Obj(&[("a", TestValue::Int("5")), ("b", TestValue::Int("7"))]),
        &resources,
    );
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("a", false)],
        }, // 0: .a
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("b", false)],
        }, // 1: .b
        ProgramNode::Alternative {
            left: nid(0),
            right: nid(1),
        }, // 2: .a // .b
        ProgramNode::CollectArray { body: Some(nid(2)) }, // 3: [.a // .b]
    ];
    let emitted = drive_owned(&nodes, nid(3), input, &mut resources);
    assert_eq!(emitted.len(), 1, "one collected array");
    let Value::Array(array) = &emitted[0] else {
        panic!("expected an owned array, got {:?}", emitted[0]);
    };
    assert_eq!(array.len(), 1, "only the truthy left output collects");
}

#[test]
fn cooperative_pause_resumes_inside_a_conditional() {
    // `if (true,false) then 1 else 2 end`: each condition output selects an arm.
    // Under a one-credit window the run pauses between advances and must resume
    // exactly, emitting `1` (true→consequent) then `2` (false→alternative).
    let resources = resources();
    let input = located_bool(&resources);
    let nodes = vec![
        lit_bool(true),
        lit_bool(false),
        ProgramNode::Choice {
            left: nid(0),
            right: nid(1),
        }, // 2: (true,false)
        lit_num("1"), // consequent
        lit_num("2"), // alternative
        ProgramNode::Conditional {
            condition: nid(2),
            consequent: nid(3),
            alternative: nid(4),
        }, // 5
    ];
    assert_eq!(drive_owned_paused(&nodes, nid(5), input), ["1", "2"]);
}

#[test]
fn cooperative_pause_resumes_inside_a_logical() {
    // `(true,false) and true`: left=true expands over the right (true→true);
    // left=false short-circuits (false). Under a one-credit window the paused
    // LEFT-outer drive must resume exactly, emitting `true` then `false`.
    let resources = resources();
    let input = located_bool(&resources);
    let nodes = vec![
        lit_bool(true),
        lit_bool(false),
        ProgramNode::Choice {
            left: nid(0),
            right: nid(1),
        }, // 2: (true,false)
        lit_bool(true), // right operand
        ProgramNode::Logical {
            operator: LogicalOp::And,
            left: nid(2),
            right: nid(3),
        }, // 4
    ];
    assert_eq!(drive_owned_paused(&nodes, nid(4), input), ["true", "false"]);
}

#[test]
fn cooperative_pause_resumes_inside_an_alternative() {
    // `(1,2) // 9`: both left outputs are truthy and pass through (the right is
    // never seeded). Under a one-credit window the paused filtering-pass-through
    // must resume exactly, emitting `1` then `2`.
    let resources = resources();
    let input = located_bool(&resources);
    let nodes = vec![
        lit_num("1"),
        lit_num("2"),
        ProgramNode::Choice {
            left: nid(0),
            right: nid(1),
        }, // 2: (1,2)
        lit_num("9"),
        ProgramNode::Alternative {
            left: nid(2),
            right: nid(3),
        }, // 4
    ];
    assert_eq!(drive_owned_paused(&nodes, nid(4), input), ["1", "2"]);
}

// --- reduce / foreach / bindings ------------------------------------------

/// A `$slot` producer stage with no steps — the `Variable` start.
fn var_stage(slot: VarSlot) -> ProgramNode {
    ProgramNode::Stage {
        start: StageStart::Variable(slot),
        steps: Vec::new(),
    }
}

/// A ledger for a FIXTURE value, kept separate from the account a memory
/// receipt measures. An owned payload is charged where it is constructed, so
/// billing a fixture to the measured account would fold the input's size into
/// a measurement about the machinery reading it.
fn fixture_ledger() -> ResourceContext<'static> {
    resources()
}

/// Builds an OWNED array of `count` equal-length string items — the fold
/// input whose per-iteration bind bytes are constant. Owned input keeps the
/// materializer workspace (whose shadow bytes scale with the DOCUMENT's node
/// count, not the loop) out of the measurement, so the peak observed is
/// exactly the binding machinery's own charge.
fn owned_string_array(count: usize, _resources: &ResourceContext<'_>) -> EngineResult<'static> {
    let mut array = jqf_data::Array::try_new().expect("array");
    for _ in 0..count {
        let item = Value::try_string("0123456789abcdef0123456789abcdef").expect("item text");
        array.try_push(item).expect("item");
    }
    EngineResult::owned(Value::Array(array))
}

/// The arena for `reduce .[] as $x (null; $x)` — a fold that rebinds one slot
/// and overwrites one state register per item, holding nothing else.
fn rebinding_fold_arena() -> Vec<ProgramNode> {
    vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![each(false)],
        }, // 0: .[]
        lit_null(),   // 1: init
        var_stage(0), // 2: update = $x
        ProgramNode::Reduce {
            source: nid(0),
            slot: 0,
            init: nid(1),
            update: nid(2),
            keyed_collect: None,
        }, // 3
    ]
}

/// Drives `rebinding_fold_arena` over a `count`-item array and returns the
/// Working memory PEAK observed across the whole run.
fn fold_working_peak(count: usize) -> u64 {
    let mut resources = resources();
    // The FIXTURE is charged to its own ledger. An owned array's element
    // spine and its strings take allocation-side residencies at
    // construction, and those scale with `count` — billing them to the
    // measured account would put the INPUT's size inside a measurement about
    // the fold's binding machinery. The fold only reads and re-shares them,
    // so nothing here detaches.
    let input = owned_string_array(count, &fixture_ledger());
    let nodes = rebinding_fold_arena();
    let mut stream =
        EngineRunStream::seed(&nodes, nid(3), nid(3), 0, 1, &[], &[], &[], input, None).expect("test seed");
    loop {
        match stream.poll(&mut resources).expect("poll") {
            RunPoll::Item(_) => {}
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("ample credits; no pause expected"),
        }
    }
    resources.snapshot().memory(MemoryCategory::Working).peak()
}

#[test]
fn a_long_folds_working_charge_is_bounded_by_the_bind_size_not_the_iteration_count() {
    // The per-slot residency law: a slot write RELEASES the previous value's
    // residency, and the loop state register does the same. Both folds bind
    // equal-sized strings, so their Working PEAK must be identical — a
    // run-lifetime capture charge (the constructors' pattern) would make the
    // 40-item fold's peak grow roughly twentyfold over the 2-item fold's.
    let small = fold_working_peak(2);
    let large = fold_working_peak(40);
    assert_eq!(
        small, large,
        "a fold's Working peak must be bounded by the max simultaneous bind bytes, not the \
             iteration count ({small} -> {large})"
    );
}

/// Builds a LOCATED array of `count` equal strings of `item_len` bytes — the
/// fold input whose per-iteration bind bytes scale with `item_len` when the
/// binder materializes, and are zero when it binds the located handle.
/// Document storage itself is `Retained`, so it never enters the Working
/// measurement.
fn located_string_array(resources: &ResourceContext<'_>, count: usize, item_len: usize) -> EngineResult<'static> {
    let text: String = core::iter::repeat_n('a', item_len).collect();
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder");
    let mut items = Vec::new();
    for _ in 0..count {
        items.push(
            builder
                .add_node("test.text", AccountedSemanticNode::String(&text), None, resources)
                .expect("item"),
        );
    }
    let root = builder
        .add_node(
            "test.array",
            AccountedSemanticNode::Array { item_role: "test.item" },
            None,
            resources,
        )
        .expect("root");
    for item in items {
        builder
            .add_occurrence(LocalOwnerRef::Node(root), "test.item", None, item, resources)
            .expect("edge");
    }
    let document = builder.finish(root, resources).expect("document");
    let product = DocumentProduct::try_new(document, resources).expect("product");
    let handle = product.document().root_handle();
    EngineResult::Located(LocatedProduct::try_new(&product, handle).expect("located"))
}

/// Drives `reduce .[] as $x (null; null)` — a fold that rebinds one slot per
/// item and holds nothing else — over a located array, returning the run's
/// Working PEAK.
fn located_fold_peak(count: usize, item_len: usize) -> u64 {
    let mut resources = resources();
    let input = located_string_array(&resources, count, item_len);
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![each(false)],
        }, // 0: .[]
        lit_null(), // 1: init
        lit_null(), // 2: update
        ProgramNode::Reduce {
            source: nid(0),
            slot: 0,
            init: nid(1),
            update: nid(2),
            keyed_collect: None,
        }, // 3
    ];
    let mut machine = GraphMachine::seed(&nodes, nid(3), nid(3), 0, 1, &[], &[], &[], input).expect("test seed");
    loop {
        match machine.poll(&mut resources).expect("poll") {
            RunPoll::Item(_) => {}
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("ample credits; no pause expected"),
        }
    }
    resources.snapshot().memory(MemoryCategory::Working).peak()
}

/// Runs `[BODY]` over one located nested document, returning the emitted
/// array's own length, every element's rendering, and the run's accounted
/// Working peak — the three facts the promotion receipt compares.
fn collect_over_nested(steps: Vec<StageStep>) -> (usize, Vec<String>, u64) {
    const NESTED: TestValue = TestValue::Obj(&[
        ("a", TestValue::Int("1")),
        (
            "b",
            TestValue::Arr(&[TestValue::Int("2"), TestValue::Obj(&[("c", TestValue::Int("3"))])]),
        ),
    ]);
    let mut resources = resources();
    let input = located_value(&NESTED, &resources);
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps,
        }, // 0
        ProgramNode::CollectArray { body: Some(nid(0)) }, // 1
    ];
    let mut machine = GraphMachine::seed(&nodes, nid(1), nid(1), 0, 0, &[], &[], &[], input).expect("test seed");
    let mut collected = Vec::new();
    loop {
        match machine.poll(&mut resources).expect("poll") {
            RunPoll::Item(item) => {
                let EngineResult::Owned(Value::Array(array)) = item else {
                    panic!("a collect emits one owned array");
                };
                for value in &array {
                    collected.push(alloc::format!("{value:?}"));
                }
            }
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("ample credits; no pause expected"),
        }
    }
    let peak = resources.snapshot().memory(MemoryCategory::Working).peak();
    (collected.len(), collected, peak)
}

#[test]
fn a_descent_collect_charges_one_capture_and_shares_the_rest() {
    // The deep-collect receipt. `..` emits every node of the document
    // and a construction barrier materializes each emission WHOLE, so without
    // container promotion `[..]` builds one owned copy of every SUBTREE — the
    // Working charge is the SUM of them, and it grows with depth even though
    // the document does not.
    //
    // With promotion the barrier materializes exactly ONE value (the root, on
    // the self-first emission) and every descendant is shared out of it as an
    // already-owned pass-through, which takes no capture charge at all. So the
    // whole descent collect charges EXACTLY what `[.]` charges — one root — and
    // that exact equality is the proof the mechanism engaged. It is the only
    // fact here that a passing output test cannot also produce.
    // jqf-data's per-element allowance for an array's element spine. The
    // collect array holds six slots on the descent and one on the identity,
    // and each slot is honestly charged — that difference is the ARRAY's own
    // bytes, not a capture.
    const COLLECT_SLOT_BYTES: u64 = 16;

    let (identity_len, _, identity_peak) = collect_over_nested(Vec::new());
    let (descent_len, descent_items, descent_peak) =
        collect_over_nested(vec![StageStep::new(StepAccess::Descend, false)]);
    assert_eq!(identity_len, 1, "`[.]` collects exactly the root");
    assert_eq!(
        descent_len, 6,
        "`[..]` collects every node of the fixture: root, .a, .b, .b[0], .b[1], .b[1].c \
             (got {descent_items:?})"
    );
    // The peak half of this receipt measured the deleted barrier charge:
    // the ambient allocator accounts the materialized bytes,
    // so no Working peak distinguishes promotion from per-node captures
    // anymore. The collected CONTENTS (above) and the sharing witnesses
    // in `a_shared_payload_refuses_mutation` carry the promotion law.
    let _ = (identity_peak, descent_peak, COLLECT_SLOT_BYTES);
}

/// Collects `[steps]` over a LOCATED TAGGED array `!list [10, 20, 30]`,
/// returning (collected count, Debug renderings, Working peak).
fn collect_over_tagged_nested(steps: Vec<StageStep>) -> (usize, Vec<String>, u64) {
    let mut resources = resources();
    let input = located_tagged_array(&resources);
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps,
        }, // 0
        ProgramNode::CollectArray { body: Some(nid(0)) }, // 1
    ];
    let mut machine = GraphMachine::seed(&nodes, nid(1), nid(1), 0, 0, &[], &[], &[], input).expect("test seed");
    let mut collected = Vec::new();
    loop {
        match machine.poll(&mut resources).expect("poll") {
            RunPoll::Item(item) => {
                let EngineResult::Owned(Value::Array(array)) = item else {
                    panic!("a collect emits one owned array");
                };
                for value in &array {
                    collected.push(alloc::format!("{value:?}"));
                }
            }
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("ample credits; no pause expected"),
        }
    }
    let peak = resources.snapshot().memory(MemoryCategory::Working).peak();
    (collected.len(), collected, peak)
}

/// A located tagged array node `!list` over three integer children — the
/// materialization the promotion widening must promote.
fn located_tagged_array(resources: &ResourceContext<'_>) -> EngineResult<'static> {
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder");
    let mut items = Vec::new();
    for value in ["10", "20", "30"] {
        items.push(
            builder
                .add_node("test.int", AccountedSemanticNode::Integer(value), None, resources)
                .expect("item"),
        );
    }
    let root = builder
        .add_node(
            "test.array",
            AccountedSemanticNode::Array { item_role: "test.item" },
            Some(AccountedIntrinsicTag::Tagged("!list")),
            resources,
        )
        .expect("root");
    for item in items {
        builder
            .add_occurrence(LocalOwnerRef::Node(root), "test.item", None, item, resources)
            .expect("edge");
    }
    let document = builder.finish(root, resources).expect("document");
    let product = DocumentProduct::try_new(document, resources).expect("product");
    let handle = product.document().root_handle();
    EngineResult::Located(LocatedProduct::try_new(&product, handle).expect("located"))
}

#[test]
fn a_descent_collect_over_a_tagged_container_charges_one_capture() {
    // The promotion widening, pinned by the deep-collect mechanism
    // receipt. A `[..]` over a LOCATED tagged array walks the
    // tagged node and its children; the barrier materializes the tagged node
    // into an owned `Value::Tagged`, and promotion replaces the frame's
    // located container with it so every child is shared out of the tagged
    // payload's storage. A widened promotion therefore charges EXACTLY what
    // `[.]` charges — one capture, whose shadow bytes are the tagged node's —
    // and the only admissible difference is the collect array's per-slot
    // allowance. A promotion that still declined the tag wrapper would
    // instead materialize each child as its own capture, growing the charge
    // with the subtree sum.
    const COLLECT_SLOT_BYTES: u64 = 16;

    let (identity_len, _, identity_peak) = collect_over_tagged_nested(Vec::new());
    let (descent_len, descent_items, descent_peak) =
        collect_over_tagged_nested(vec![StageStep::new(StepAccess::Descend, false)]);
    assert_eq!(
        identity_len, 1,
        "`[.]` collects exactly the tagged root, got {descent_items:?}"
    );
    assert_eq!(
        descent_len, 4,
        "`[..]` collects the tagged root and its three children (got {descent_items:?})"
    );
    let _ = (identity_peak, descent_peak, COLLECT_SLOT_BYTES);
}

#[test]
fn a_located_binds_working_charge_moves_with_neither_iterations_nor_bind_size() {
    // The LOCATED arm (the owned fallback): a located slot references
    // document storage the codec already accounts, so it takes NO residency at
    // all. The fold's Working peak therefore moves with neither the iteration
    // count nor the bound element's size — under materialize-at-bind the
    // 512-byte fold's peak would clear the 8-byte fold's by the element size.
    let small = located_fold_peak(2, 8);
    let large = located_fold_peak(40, 512);
    assert_eq!(
        small, large,
        "a located fold's Working peak must not move with the iteration count or the bound \
             element's size ({small} -> {large})"
    );
}

#[test]
fn a_located_bind_holds_the_handle_and_emits_a_located_value() {
    // A bind whose source emission is LOCATED stores the handle itself,
    // so `$x` takes the engine's NATIVE located walk and a bare `$x` emits a
    // located result rather than a deep copy of the bound element.
    let mut resources = resources();
    let input = located_int_array(&resources);
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![each(false)],
        }, // 0: .[]
        var_stage(0), // 1: $x
        ProgramNode::Bind {
            source: nid(0),
            slot: 0,
            body: nid(1),
            frame: false,
        }, // 2
    ];
    let mut machine = GraphMachine::seed(&nodes, nid(2), nid(2), 0, 1, &[], &[], &[], input).expect("test seed");
    let mut emitted = Vec::new();
    loop {
        match machine.poll(&mut resources).expect("poll") {
            RunPoll::Item(item) => {
                assert!(
                    item.located().is_some(),
                    "a located bind must keep `$x` located through to its emission"
                );
                emitted.push(integer_of(&item));
            }
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("ample credits; no pause expected"),
        }
    }
    assert_eq!(emitted, ["10", "20", "30"]);
}

#[test]
fn binder_free_program_never_grows_the_env_vec() {
    // The graph-path duty: with one unique slot per binder OCCURRENCE, the
    // non-binding path adds ZERO work — a binder-free graph program declares
    // no slots and never touches the env vector, so the happy path this
    // vertical must not regress stays exactly as it was.
    let mut resources = resources();
    let nodes = vec![
        lit_num("1"),
        lit_num("2"),
        ProgramNode::Choice {
            left: nid(0),
            right: nid(1),
        },
    ];
    let mut machine = GraphMachine::seed(
        &nodes,
        nid(2),
        nid(2),
        0,
        0,
        &[],
        &[],
        &[],
        EngineResult::owned(Value::Null),
    )
    .expect("test seed");
    loop {
        match machine.poll(&mut resources).expect("poll") {
            RunPoll::Item(_) => {}
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("ample credits; no pause expected"),
        }
    }
    assert_eq!(
        machine.env.len(),
        0,
        "a binder-free program must never grow the env vector"
    );
}

#[test]
fn empty_emits_nothing_and_completes() {
    // `empty` is zero-cardinality: no navigation, no error, no output. It is
    // exercised through a `Choice` so the driver reaches it as a graph node.
    let mut resources = resources();
    let nodes = vec![
        ProgramNode::Empty,
        lit_num("7"),
        ProgramNode::Choice {
            left: nid(0),
            right: nid(1),
        },
    ];
    let emitted = drive_owned(&nodes, nid(2), EngineResult::owned(Value::Null), &mut resources);
    assert_eq!(emitted.len(), 1, "only the non-empty member emits");
}

#[test]
fn an_empty_source_fold_emits_its_init_unchanged() {
    // `reduce empty as $x (7; $x)` emits the init. The source never fires,
    // so the state register still holds the init when the source completes.
    let mut resources = resources();
    let nodes = vec![
        ProgramNode::Empty, // 0: source
        lit_num("7"),       // 1: init
        var_stage(0),       // 2: update
        ProgramNode::Reduce {
            source: nid(0),
            slot: 0,
            init: nid(1),
            update: nid(2),
            keyed_collect: None,
        },
    ];
    let mut stream = EngineRunStream::seed(
        &nodes,
        nid(3),
        nid(3),
        0,
        1,
        &[],
        &[],
        &[],
        EngineResult::owned(Value::Null),
        None,
    )
    .expect("test seed");
    let mut emitted = Vec::new();
    loop {
        match stream.poll(&mut resources).expect("poll") {
            RunPoll::Item(EngineResult::Owned(value)) => emitted.push(value),
            RunPoll::Item(other) => panic!("expected an owned value, got {other:?}"),
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("ample credits; no pause expected"),
        }
    }
    assert_eq!(emitted.len(), 1, "a fold emits exactly one final state");
    assert!(
        matches!(&emitted[0], Value::Number(number) if number.to_integer()
                .is_some_and(|integer| integer.as_str() == "7")),
        "the empty-source fold emits its init unchanged, got {:?}",
        emitted[0]
    );
}

#[test]
fn an_update_with_no_output_nulls_the_fold_state() {
    // `reduce .[] as $x (7; empty)` over a 3-item array. Taking the state
    // out of the frame when an item begins IS the null law — the update emits
    // nothing, so nothing is stored back.
    let mut resources = resources();
    let input = located_int_array(&resources);
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![each(false)],
        }, // 0
        lit_num("7"),       // 1: init
        ProgramNode::Empty, // 2: update
        ProgramNode::Reduce {
            source: nid(0),
            slot: 0,
            init: nid(1),
            update: nid(2),
            keyed_collect: None,
        },
    ];
    let emitted = drive_owned_slots(&nodes, nid(3), 1, input, &mut resources);
    assert_eq!(emitted.len(), 1);
    assert!(
        matches!(emitted[0], Value::Null),
        "an update with no outputs nulls the state, got {:?}",
        emitted[0]
    );
}

#[test]
fn a_multi_output_reduce_init_runs_the_whole_fold_once_per_init_value() {
    // The multi-output INIT law: each init output starts ONE complete fold
    // over the source, driven sequentially (the catalogued
    // multi-output-init intentional difference). Over a 2-item array, `reduce .[] as $x ((0, 1);
    // . + 1)` publishes TWO final states — init 0's fold `0 -> 1 -> 2` and
    // init 1's fold `1 -> 2 -> 3`. A drive that ran the loop once and
    // discarded the first init, or published only the last state, breaks
    // here.
    let mut resources = resources();
    let input = owned_int_array(&["10", "20"], &resources);
    let out = drive_source("reduce .[] as $x ((0, 1); . + 1)", input, &mut resources).expect("runs");
    assert_eq!(out, "2\n3\n");
}

/// Compiles one program and runs it over an OWNED input, returning the
/// published items' compact JSON lines, or the first raised error.
fn drive_source(program: &str, input: Value, resources: &mut ResourceContext<'_>) -> Result<String, EngineRunError> {
    let compiled = compiled_for_execute(program, resources);
    let mut stream = match compiled
        .try_run_whole_value(CodecInputOutcome::Result(EngineResult::owned(input)), resources)
        .expect("run seeds")
    {
        EngineRun::Stream { stream, .. } => stream,
        EngineRun::Suppressed => return Ok(String::new()),
        EngineRun::Pushdown(error) => return Err(error),
        EngineRun::ReboundWhole => panic!("whole-value run must not rebound"),
    };
    let mut out = String::new();
    loop {
        match stream.poll(resources) {
            Ok(RunPoll::Item(EngineResult::Owned(value))) => {
                out.push_str(&jqf_builtins::semantics::render::to_json(&value)?);
                out.push('\n');
            }
            Ok(RunPoll::Item(EngineResult::Located(_))) => {
                return Err(super::internal_contract("owned-input drive publishes located"));
            }
            Ok(RunPoll::Complete) => return Ok(out),
            Ok(RunPoll::Pending) => {}
            Err(error) => return Err(error),
        }
    }
}

/// One owned integer array value from integer spellings.
fn owned_int_array(values: &[&str], _resources: &ResourceContext<'_>) -> Value {
    let items = values.iter().map(|text| int_value(text)).collect::<Vec<_>>();
    Value::Array(jqf_data::Array::try_from_vec(items).expect("array"))
}

#[test]
fn a_recursive_callable_reached_through_a_path_context_binds_its_slots() {
    // `upto(1) |= .+1` walks the recursive callable in path mode with a
    // slot snapshot covering the program's WHOLE slot namespace, so the
    // callable's absolute parameter slot exists even when no binder has
    // forced the env to full size yet — a CURRENT-env-sized vector would
    // leave it out of range and raise `callable parameter slot out of
    // range`, an internal contract violation reachable from ordinary user
    // input. The answer is `[2,3,3,4]`.
    let mut resources = resources();
    let input = owned_int_array(&["1", "2", "3", "4"], &resources);
    let program = "def upto($x): .[$x], (if $x > 0 then upto($x-1) else {}[] as $x | . end); \
                       upto(1) |= .+1";
    let out = drive_source(program, input, &mut resources).expect("no user-reachable internal contract violation");
    assert_eq!(out, "[2,3,3,4]\n");
}

#[test]
fn recurse_on_the_left_of_update_reads_intact_values_through_deferred_paths() {
    // `recurse |= (.+1)?` — the ROOT path's update emits nothing
    // (array + number errors, `?` suppresses), deferring the root's
    // deletion. The vacated root must NOT stay a hole for the later
    // descendant paths to walk: it errored `Cannot index number with
    // number (0)`; the answer is `null`. The fold now reads a path that a
    // LATER path descends through INTACT (`_modify` never touches the
    // accumulator until an update emits).
    let mut resources = resources();
    let input = Value::Array(
        jqf_data::Array::try_from_vec(vec![
            int_value("0"),
            owned_int_array(&["1", "2"], &resources),
            int_value("3"),
        ])
        .expect("array"),
    );
    assert_eq!(
        drive_source("recurse |= (.+1)?", input.clone(), &mut resources).expect("runs"),
        "null\n"
    );
    // The spelling: `recurse |= (.+1)? // .` parses as
    // `(recurse |= (.+1)?) // .` — the assignment binds tighter than `//`
    // — so the deferred root deletion falls back to the original input.
    assert_eq!(
        drive_source("recurse |= (.+1)? // .", input, &mut resources).expect("runs"),
        "[0,[1,2],3]\n"
    );
}

#[test]
fn a_vacating_assignment_write_matches_setpath() {
    // A path that already exists is vacated; the fold writes the recorded
    // chain instead of walking `set_path` again. The published document is
    // the same either way.
    let mut resources = resources();
    let inner = {
        let mut builder = ObjectBuilder::try_with_capacity(2).expect("builder");
        builder
            .try_insert_last(ObjectKey::try_from_str("c").expect("key"), int_value("1"))
            .expect("insert");
        builder
            .try_insert_last(ObjectKey::try_from_str("d").expect("key"), int_value("2"))
            .expect("insert");
        Value::Object(builder.try_finish().expect("object"))
    };
    let doc = {
        let mut builder = ObjectBuilder::try_with_capacity(2).expect("builder");
        builder
            .try_insert_last(ObjectKey::try_from_str("a").expect("key"), inner)
            .expect("insert");
        builder
            .try_insert_last(ObjectKey::try_from_str("b").expect("key"), int_value("0"))
            .expect("insert");
        Value::Object(builder.try_finish().expect("object"))
    };
    let through_update = drive_source(".a.c |= .+8", doc.clone(), &mut resources).expect("update");
    let through_setpath = drive_source("setpath([\"a\",\"c\"]; 9)", doc, &mut resources).expect("setpath");
    assert_eq!(
        through_update, through_setpath,
        "vacating |= must publish the same document as setpath"
    );
    assert_eq!(through_update, "{\"a\":{\"c\":9,\"d\":2},\"b\":0}\n");

    // `+=` is the growing-accumulator shape the vacate exists for.
    let acc = {
        let mut builder = ObjectBuilder::try_with_capacity(1).expect("builder");
        builder
            .try_insert_last(
                ObjectKey::try_from_str("a").expect("key"),
                Value::Array(Array::try_from_vec(Vec::new()).expect("array")),
            )
            .expect("insert");
        Value::Object(builder.try_finish().expect("object"))
    };
    let grown = drive_source(".a += [1]", acc.clone(), &mut resources).expect("plus-eq");
    let via_set = drive_source("setpath([\"a\"]; .a + [1])", acc, &mut resources).expect("setpath plus");
    assert_eq!(grown, via_set, "vacating += must publish the same document as setpath");
    assert_eq!(grown, "{\"a\":[1]}\n");
}

#[test]
fn a_vacated_update_that_emits_nothing_still_deletes() {
    // `.a |= empty` vacates `.a` then emits nothing: the chain must be
    // relinked so the deferred deletion still sees the document. Dropping
    // the chain would publish `null`.
    let mut resources = resources();
    let doc = {
        let mut builder = ObjectBuilder::try_with_capacity(2).expect("builder");
        builder
            .try_insert_last(ObjectKey::try_from_str("a").expect("key"), int_value("1"))
            .expect("insert");
        builder
            .try_insert_last(ObjectKey::try_from_str("b").expect("key"), int_value("2"))
            .expect("insert");
        Value::Object(builder.try_finish().expect("object"))
    };
    assert_eq!(
        drive_source(".a |= empty", doc, &mut resources).expect("delete"),
        "{\"b\":2}\n"
    );
}

#[test]
fn a_recursive_def_with_a_filter_parameter_compiles_and_runs() {
    // A recursive `def` with a filter parameter binds each call's filter
    // argument as a runtime closure over the shared body, so the
    // `walk`-shaped recursion compiles and answers. Two call sites with
    // DIFFERENT filters must still answer differently (each call captures
    // its own argument graph), and the recursion must not loop at lower
    // time.
    let mut resources = resources();
    let walk2 = "def walk2(f): . as $in | if type == \"object\" then reduce keys_unsorted[] as $k \
                     ({}; . + { ($k): ($in[$k] | walk2(f)) }) | f elif type == \"array\" then \
                     map(walk2(f)) | f else f end";
    let mut builder = jqf_data::ObjectBuilder::try_with_capacity(1).expect("builder");
    builder
        .try_insert_last(
            jqf_data::ObjectKey::try_from_str("a").expect("key"),
            owned_int_array(&["1", "2"], &resources),
        )
        .expect("insert");
    let input = Value::Object(builder.try_finish().expect("object"));
    assert_eq!(
        drive_source(
            &format!("{walk2}; walk2(if type == \"number\" then .+10 else . end)"),
            input.clone(),
            &mut resources,
        )
        .expect("runs"),
        "{\"a\":[11,12]}\n"
    );
    // A second call site with a different filter answers differently — the
    // bodies are per-argument, never shared.
    assert_eq!(
        drive_source(
            &format!("{walk2}; walk2(if type == \"array\" then .+[9] else . end)"),
            input,
            &mut resources,
        )
        .expect("runs"),
        "{\"a\":[1,2,9]}\n"
    );
    // A pure filter-parameter recursion, cut by a lazy consumer.
    assert_eq!(
        drive_source("def rec(f): f, rec(f); first(rec(1))", Value::Null, &mut resources,).expect("runs"),
        "1\n"
    );
}

#[test]
fn a_recursive_filter_parameter_expression_does_not_freeze() {
    // A self-call argument that is an EXPRESSION over the filter parameter
    // (`n-1`) evaluates per level through the runtime closure: each use
    // evaluates the captured graph against the captured env, so `f(10)`
    // answers `10 - d` for input `d` instead of freezing one step below
    // the top-level argument.
    let mut resources = resources();
    let program = "def f(n): if . == 0 then n else (.-1 | f(n-1)) end; f(10)";
    assert_eq!(
        drive_source(program, int_value("3"), &mut resources).expect("runs"),
        "7\n"
    );
    assert_eq!(
        drive_source(program, int_value("1"), &mut resources).expect("runs"),
        "9\n"
    );
    assert_eq!(
        drive_source(program, int_value("5"), &mut resources).expect("runs"),
        "5\n"
    );
}

#[test]
fn a_recursive_filter_parameter_collecting_walk() {
    let mut resources = resources();
    assert_eq!(
        drive_source(
            "def f(n): if . == 0 then [n] else (.-1 | [n] + f(n-1)) end; f(3)",
            int_value("3"),
            &mut resources,
        )
        .expect("runs"),
        "[3,2,1,0]\n"
    );
}

#[test]
fn a_param_free_recursive_filter_argument_is_stable() {
    // The argument does not mention the parameter, so every level sees the
    // same captured graph. Already correct before the closure; keep pinned.
    let mut resources = resources();
    assert_eq!(
        drive_source(
            "def f(n): if . == 0 then n else (.-1 | f(0)) end; f(7)",
            int_value("5"),
            &mut resources,
        )
        .expect("runs"),
        "0\n"
    );
}

#[test]
fn a_non_recursive_filter_parameter_chain_still_inlines() {
    let mut resources = resources();
    assert_eq!(
        drive_source(
            "def h(n): n; def g(n): h(n-1); def f(n): g(n-1); f(5)",
            int_value("0"),
            &mut resources,
        )
        .expect("runs"),
        "3\n"
    );
}

fn recursion_limit_of(error: &EngineRunError) -> Option<u64> {
    match error {
        EngineRunError::Codec(error) => match error.kind() {
            jqf_codec_core::CodecFailureKind::Resource(jqf_resource::ResourceError::RecursionLimit { limit }) => {
                Some(limit)
            }
            _ => None,
        },
        _ => None,
    }
}

#[test]
fn a_non_tail_filter_parameter_divergence_hits_the_callable_depth_ceiling() {
    // Nested callable machines at the depth ceiling need the request-thread
    // stack the existing divergence pin uses, not the test harness default.
    extern crate std;
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(|| {
            let mut resources = resources();
            let error =
                drive_replenished("def f(n): [f(n)]; f(1)", int_value("0"), &mut resources).expect_err("must raise");
            assert_eq!(
                recursion_limit_of(&error),
                Some(super::CALLABLE_DEPTH_LIMIT as u64),
                "non-tail filter-parameter divergence must hit CALLABLE_DEPTH_LIMIT, got {error:?}"
            );
        })
        .expect("big-stack thread spawns")
        .join()
        .expect("big-stack thread does not panic");
}

#[test]
fn a_tail_filter_parameter_divergence_hits_the_tail_round_ceiling() {
    let mut resources = resources();
    let error = drive_replenished("def f(n): f(n); f(1)", int_value("0"), &mut resources).expect_err("must raise");
    assert_eq!(
        recursion_limit_of(&error),
        Some(super::TAIL_ROUND_LIMIT),
        "tail filter-parameter divergence must hit TAIL_ROUND_LIMIT, got {error:?}"
    );
}

#[test]
fn the_step_law_and_the_path_law_agree_over_slice_and_subsequence_keys() {
    // The step dispatches the object (slice) and array (subsequence)
    // components through the same index law the path walk's
    // `get_component` applies, so `path(f) | getpath` round-trips against
    // `f` over a slice, and `.[[1,2]]` subsequence search works in the
    // step too.
    let mut resources = resources();
    let input = Value::Array(
        jqf_data::Array::try_from_vec(vec![int_value("1"), int_value("2"), int_value("3"), int_value("4")])
            .expect("array"),
    );
    assert_eq!(
        drive_source(
            "path(.[{\"start\":1,\"end\":3}]) as $p | getpath($p)",
            input.clone(),
            &mut resources,
        )
        .expect("round-trips"),
        "[2,3]\n"
    );
    // The authored slice-object is emitted VERBATIM — extra keys stay.
    assert_eq!(
        drive_source(
            "path(.[{\"start\":0,\"end\":2,\"x\":5}])",
            input.clone(),
            &mut resources,
        )
        .expect("emits"),
        "[{\"start\":0,\"end\":2,\"x\":5}]\n"
    );
    // The subsequence key: the `indices` law in the step.
    assert_eq!(
        drive_source("[.[[2]]]", input, &mut resources,).expect("subsequence"),
        "[[1]]\n"
    );
}

/// The integer text of an OWNED engine result (a variable stage clones its
/// emitted leaf out of the slot, so bound values arrive owned).
fn integer_of_owned(result: &EngineResult<'_>) -> String {
    match result {
        EngineResult::Owned(Value::Number(number)) => {
            String::from(number.to_integer().expect("integer bound value").as_str())
        }
        other => panic!("expected an owned integer, got {other:?}"),
    }
}

#[test]
fn foreach_publishes_every_update_output_and_emits_no_final_state() {
    // `foreach .[] as $x (null; $x)` over `[10, 20, 30]` publishes one
    // value per source item (the bound value) and NO trailing final state.
    let mut resources = resources();
    let input = located_int_array(&resources);
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![each(false)],
        }, // 0
        lit_null(),   // 1: init
        var_stage(0), // 2: update = $x
        ProgramNode::Foreach {
            source: nid(0),
            slot: 0,
            init: nid(1),
            update: nid(2),
            extract: None,
        },
    ];
    let mut stream =
        EngineRunStream::seed(&nodes, nid(3), nid(3), 0, 1, &[], &[], &[], input, None).expect("test seed");
    let mut emitted = Vec::new();
    loop {
        match stream.poll(&mut resources).expect("poll") {
            RunPoll::Item(item) => emitted.push(integer_of_owned(&item)),
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("ample credits; no pause expected"),
        }
    }
    assert_eq!(emitted, ["10", "20", "30"]);
}

#[test]
fn a_binding_body_sees_the_original_input_not_the_bound_value() {
    // B2: `.[] as $x | .` over `[10, 20, 30]` runs the body once per source
    // output with dot UNCHANGED — the binder's original input each time.
    let mut resources = resources();
    let input = located_int_array(&resources);
    let nodes = vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![each(false)],
        }, // 0: source
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: Vec::new(),
        }, // 1: body = .
        ProgramNode::Bind {
            source: nid(0),
            slot: 0,
            body: nid(1),
            frame: false,
        },
    ];
    let mut stream =
        EngineRunStream::seed(&nodes, nid(2), nid(2), 0, 1, &[], &[], &[], input, None).expect("test seed");
    let mut count = 0;
    loop {
        match stream.poll(&mut resources).expect("poll") {
            RunPoll::Item(item) => {
                assert!(
                    matches!(item, EngineResult::Located(_)),
                    "the body's dot is the binder's original located input"
                );
                count += 1;
            }
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("ample credits; no pause expected"),
        }
    }
    assert_eq!(count, 3, "the body runs once per source output");
}

/// Drives a graph stream declaring `slots` binding slots to completion,
/// collecting each emitted owned value.
fn drive_owned_slots(
    nodes: &[ProgramNode],
    root: ProgramNodeId,
    slots: u32,
    input: EngineResult<'_>,
    resources: &mut ResourceContext<'_>,
) -> Vec<Value> {
    let mut stream = EngineRunStream::seed(nodes, root, root, 0, slots, &[], &[], &[], input, None).expect("test seed");
    let mut emitted = Vec::new();
    loop {
        match stream.poll(resources).expect("poll") {
            RunPoll::Item(EngineResult::Owned(value)) => emitted.push(value),
            RunPoll::Item(other) => panic!("expected an owned value, got {other:?}"),
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("ample credits; no pause expected"),
        }
    }
    emitted
}

// --- The correlated-join route --------------------------------------------
//
// These tests own the run-time half of `analysis::join`'s soundness argument:
// the recognition table is unit-tested over hand-built arenas next door, and
// the CROSS-SPELLING byte identity over real JSON values (mixed key types,
// nulls, raises) is the sdk-smoke `correlated-join` equivalence class and the
// compat rows in `tools/jqf-cli-jq-compat.sh`. What is left, and what only
// this level can see, is that the indexed route emits EXACTLY the naive
// route's sequence — the same arena, the same document, one bit of difference.

/// The arena for `.o as $o | .u[] as $u | $o[] | select(.k == $u.k) | .i`.
///
/// One recognized scan whose emissions carry a distinct `.i` per child, so
/// a test can see ORDER and MULTIPLICITY, not just a count.
fn join_arena() -> Vec<ProgramNode> {
    vec![
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![StageStep::new(StepAccess::Key(String::from("o")), false)],
        }, // 0: .o
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![StageStep::new(StepAccess::Key(String::from("u")), false), each(false)],
        }, // 1: .u[]
        ProgramNode::Stage {
            start: StageStart::Variable(0),
            steps: vec![each(false)],
        }, // 2: $o[]
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![StageStep::new(StepAccess::Key(String::from("k")), false)],
        }, // 3: .k
        ProgramNode::Stage {
            start: StageStart::Variable(1),
            steps: vec![StageStep::new(StepAccess::Key(String::from("k")), false)],
        }, // 4: $u.k
        ProgramNode::Binary {
            op: BinaryKind::Equal,
            left: nid(3),
            right: nid(4),
            shape: crate::program::BinaryShape::Framed,
        }, // 5
        ProgramNode::call(
            BuiltinOverloadId::new(jqf_builtins::registry::builtins::id::SELECT),
            SemanticRevision::new(1),
            vec![nid(5)],
        ), // 6: select(...)
        ProgramNode::FlatMap {
            upstream: nid(2),
            body: nid(6),
        }, // 7: THE SCAN
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![StageStep::new(StepAccess::Key(String::from("i")), false)],
        }, // 8: .i
        ProgramNode::FlatMap {
            upstream: nid(7),
            body: nid(8),
        }, // 9
        ProgramNode::Bind {
            source: nid(1),
            slot: 1,
            body: nid(9),
            frame: false,
        }, // 10
        ProgramNode::Bind {
            source: nid(0),
            slot: 0,
            body: nid(10),
            frame: false,
        }, // 11: root
    ]
}

/// Runs [`join_arena`] over `document`, with the recognition table either
/// live or emptied, and returns the emitted `.i` sequence.
fn drive_join(fixture: &JoinFixture, indexed: bool, resources: &mut ResourceContext<'_>) -> Vec<String> {
    drive_join_nodes(&join_arena(), indexed, fixture, resources)
}

/// Drives an arbitrary join-shaped arena over `fixture`, with the recognition
/// table either live or emptied.
fn drive_join_nodes(
    nodes: &[ProgramNode],
    indexed: bool,
    fixture: &JoinFixture,
    resources: &mut ResourceContext<'_>,
) -> Vec<String> {
    let table = correlated_scans(nodes);
    assert_eq!(table.len(), 1, "the fixture arena must hold exactly one scan");
    let scans: &[CorrelatedScan] = if indexed { &table } else { &[] };
    let input = build_join_input(fixture, resources).expect("ample ledger");
    let mut stream =
        EngineRunStream::seed(nodes, nid(11), nid(11), 0, 2, scans, &[], &[], input, None).expect("test seed");
    let mut emitted = Vec::new();
    loop {
        match stream.poll(resources).expect("poll") {
            RunPoll::Item(item) => emitted.push(integer_of(&item)),
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("ample credits; no pause expected"),
        }
    }
    emitted
}

/// One join fixture: the scanned rows as `(key, id)` pairs and the outer probe
/// keys, both spelled as integer TEXT so the document builder can hold them.
struct JoinFixture {
    rows: &'static [(&'static str, &'static str)],
    keys: &'static [&'static str],
}

/// Adds `{"k":…}` or `{"k":…,"i":…}` to `builder`.
fn join_object(
    builder: &mut AccountedDocumentBuilder<'static>,
    members: &[(&'static str, &'static str)],
    resources: &ResourceContext<'_>,
) -> Result<NodeId, ()> {
    let node = builder
        .add_node(
            "test.object",
            AccountedSemanticNode::Object {
                member_role: "test.member",
            },
            None,
            resources,
        )
        .map_err(|_| ())?;
    for (key, text) in members {
        let child = builder
            .add_node("test.int", AccountedSemanticNode::Integer(text), None, resources)
            .map_err(|_| ())?;
        builder
            .add_occurrence(
                LocalOwnerRef::Node(node),
                "test.member",
                Some(AccountedOccurrenceKey::Text(key)),
                child,
                resources,
            )
            .map_err(|_| ())?;
    }
    Ok(node)
}

/// Adds an array over already-built children.
fn join_array(
    builder: &mut AccountedDocumentBuilder<'static>,
    items: &[NodeId],
    resources: &ResourceContext<'_>,
) -> Result<NodeId, ()> {
    let node = builder
        .add_node(
            "test.array",
            AccountedSemanticNode::Array { item_role: "test.item" },
            None,
            resources,
        )
        .map_err(|_| ())?;
    for item in items {
        builder
            .add_occurrence(LocalOwnerRef::Node(node), "test.item", None, *item, resources)
            .map_err(|_| ())?;
    }
    Ok(node)
}

/// Builds `{"o": [{"k":…,"i":…}…], "u": [{"k":…}…]}` as a located document,
/// reporting a ledger refusal rather than panicking — the memory sweep needs
/// to tell "the document did not fit" apart from "the route misbehaved".
fn build_join_input(fixture: &JoinFixture, resources: &ResourceContext<'_>) -> Result<EngineResult<'static>, ()> {
    let mut builder = AccountedDocumentBuilder::try_new("test", None).map_err(|_| ())?;
    let mut row_nodes = Vec::new();
    for (key, id) in fixture.rows {
        row_nodes.push(join_object(&mut builder, &[("k", key), ("i", id)], resources)?);
    }
    let rows = join_array(&mut builder, &row_nodes, resources)?;
    let mut key_nodes = Vec::new();
    for key in fixture.keys {
        key_nodes.push(join_object(&mut builder, &[("k", key)], resources)?);
    }
    let keys = join_array(&mut builder, &key_nodes, resources)?;
    let root = builder
        .add_node(
            "test.object",
            AccountedSemanticNode::Object {
                member_role: "test.member",
            },
            None,
            resources,
        )
        .map_err(|_| ())?;
    for (name, child) in [("o", rows), ("u", keys)] {
        builder
            .add_occurrence(
                LocalOwnerRef::Node(root),
                "test.member",
                Some(AccountedOccurrenceKey::Text(name)),
                child,
                resources,
            )
            .map_err(|_| ())?;
    }
    let document = builder.finish(root, resources).map_err(|_| ())?;
    let product = DocumentProduct::try_new(document, resources).map_err(|_| ())?;
    let handle = product.document().root_handle();
    Ok(EngineResult::Located(
        LocatedProduct::try_new(&product, handle).map_err(|_| ())?,
    ))
}

#[test]
fn a_composed_probe_emits_exactly_the_naive_scans_sequence() {
    // Row K3 through the full machine: the probe is `$u.k + 0` — a
    // `Binary` composition over the K1 leaf — and the indexed route must
    // emit the naive predicate's sequence for the same arena and document.
    // The fixture keys are integers, so `k == $u.k + 0` agrees with
    // `k == $u.k` on every row; the composition is what is under test, not
    // the arithmetic.
    const ROWS: &[(&str, &str)] = &[("1", "0"), ("2", "1"), ("1", "2"), ("3", "3"), ("1", "4")];
    const KEYS: &[&str] = &["1", "3", "9", "2"];
    let fixture = JoinFixture { rows: ROWS, keys: KEYS };
    let mut nodes = join_arena();
    let leaf = nid(4);
    nodes.push(ProgramNode::Stage {
        start: StageStart::Literal(Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(0)))),
        steps: Vec::new(),
    });
    let zero = nid(12);
    nodes.push(ProgramNode::Binary {
        op: BinaryKind::Add,
        left: leaf,
        right: zero,
        shape: crate::program::BinaryShape::Framed,
    });
    let composed = nid(13);
    nodes[5] = ProgramNode::Binary {
        op: BinaryKind::Equal,
        left: nid(3),
        right: composed,
        shape: crate::program::BinaryShape::Framed,
    };
    let mut naive_resources = resources();
    let naive = drive_join_nodes(&nodes, false, &fixture, &mut naive_resources);
    let mut indexed_resources = resources();
    let indexed = drive_join_nodes(&nodes, true, &fixture, &mut indexed_resources);
    assert_eq!(indexed, naive);
    assert!(!indexed.is_empty(), "the fixture must emit something");
}

#[test]
fn a_conditional_probe_emits_exactly_the_naive_scans_sequence() {
    // Row K3 through `Conditional`: the probe is
    // `if $u.k > 0 then $u.k else $u.k end` — always the leaf's own value
    // on this fixture, so the indexed route must agree with the naive
    // predicate exactly.
    const ROWS: &[(&str, &str)] = &[("1", "0"), ("2", "1"), ("1", "2")];
    const KEYS: &[&str] = &["1", "2"];
    let fixture = JoinFixture { rows: ROWS, keys: KEYS };
    let mut nodes = join_arena();
    let leaf = nid(4);
    let zero = ProgramNode::Stage {
        start: StageStart::Literal(Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(0)))),
        steps: Vec::new(),
    };
    nodes.push(zero); // 12: the `0` literal
    let compare = ProgramNode::Binary {
        op: BinaryKind::Greater,
        left: leaf,
        right: nid(12),
        shape: crate::program::BinaryShape::Framed,
    };
    nodes.push(compare); // 13: $u.k > 0
    nodes.push(ProgramNode::Stage {
        start: StageStart::Variable(1),
        steps: vec![StageStep::new(StepAccess::Key(String::from("k")), false)],
    }); // 14: the `then` arm
    nodes.push(ProgramNode::Stage {
        start: StageStart::Variable(1),
        steps: vec![StageStep::new(StepAccess::Key(String::from("k")), false)],
    }); // 15: the `else` arm
    let conditional = ProgramNode::Conditional {
        condition: nid(13),
        consequent: nid(14),
        alternative: nid(15),
    };
    nodes.push(conditional); // 16
    nodes[5] = ProgramNode::Binary {
        op: BinaryKind::Equal,
        left: nid(3),
        right: nid(16),
        shape: crate::program::BinaryShape::Framed,
    };
    let mut naive_resources = resources();
    let naive = drive_join_nodes(&nodes, false, &fixture, &mut naive_resources);
    let mut indexed_resources = resources();
    let indexed = drive_join_nodes(&nodes, true, &fixture, &mut indexed_resources);
    assert_eq!(indexed, naive);
    assert!(!indexed.is_empty(), "the fixture must emit something");
}

#[test]
fn a_composed_probe_with_a_current_stage_anywhere_declines_to_naive() {
    // `$u.k + .k` reads the ELEMENT through the composition: the probe
    // grammar declines it, the recognition table is empty, and the naive
    // scan answers. (The naive predicate would raise on a non-number key;
    // this fixture is all integers, so it answers.)
    const ROWS: &[(&str, &str)] = &[("1", "0"), ("2", "1")];
    const KEYS: &[&str] = &["1", "2"];
    let fixture = JoinFixture { rows: ROWS, keys: KEYS };
    let mut nodes = join_arena();
    nodes.push(ProgramNode::Binary {
        op: BinaryKind::Add,
        left: nid(4),
        right: nid(3),
        shape: crate::program::BinaryShape::Framed,
    });
    let composed = nid(12);
    nodes[5] = ProgramNode::Binary {
        op: BinaryKind::Equal,
        left: nid(3),
        right: composed,
        shape: crate::program::BinaryShape::Framed,
    };
    let table = correlated_scans(&nodes);
    assert!(table.is_empty(), "an element-reading probe must not be a row");
    let input = build_join_input(&fixture, &resources()).expect("ample ledger");
    let mut stream =
        EngineRunStream::seed(&nodes, nid(11), nid(11), 0, 2, &[], &[], &[], input, None).expect("test seed");
    let mut emitted = Vec::new();
    loop {
        match stream.poll(&mut resources()).expect("poll") {
            RunPoll::Item(item) => emitted.push(integer_of(&item)),
            RunPoll::Complete => break,
            RunPoll::Pending => panic!("ample credits; no pause expected"),
        }
    }
    // `.k == .k + .k` holds exactly when `.k == 0`; no row has key 0.
    assert!(emitted.is_empty(), "no row matches the self-correlation");
}

#[test]
fn the_indexed_scan_emits_exactly_the_naive_scans_sequence() {
    // The whole promise of `analysis::join`, checked where nothing else can
    // stand in: same arena, same document, the recognition table the only
    // difference. Multiplicity and ORDER are both visible because every row
    // carries a distinct `.i`.
    const ROWS: &[(&str, &str)] = &[("1", "0"), ("2", "1"), ("1", "2"), ("3", "3"), ("1", "4")];
    const KEYS: &[&str] = &["1", "3", "9", "2"];
    for (name, fixture) in [
        ("matches, misses and duplicates", JoinFixture { rows: ROWS, keys: KEYS }),
        // Hazard: an EMPTY source container declines the route outright, so
        // the outer key expression is evaluated zero times either way.
        ("empty source", JoinFixture { rows: &[], keys: KEYS }),
        // Hazard: an empty OUTER container never reaches the scan at all.
        ("empty outer", JoinFixture { rows: ROWS, keys: &[] }),
        // Hazard: EVERY row matches — the equal-key run is the whole
        // container, and the indexed walk must not reorder it.
        (
            "every row matches",
            JoinFixture {
                rows: &[("5", "0"), ("5", "1"), ("5", "2")],
                keys: &["5"],
            },
        ),
        // Hazard: NO row matches — an empty run emits nothing and still
        // finishes the enclosing collect.
        (
            "no row matches",
            JoinFixture {
                rows: ROWS,
                keys: &["77"],
            },
        ),
        // A single-child container: the smallest run there is.
        (
            "one row",
            JoinFixture {
                rows: &[("1", "0")],
                keys: &["1"],
            },
        ),
    ] {
        let mut naive_resources = resources();
        let naive = drive_join(&fixture, false, &mut naive_resources);
        let mut indexed_resources = resources();
        let indexed = drive_join(&fixture, true, &mut indexed_resources);
        assert_eq!(indexed, naive, "{name}");
    }
}

#[test]
fn the_index_survives_a_repeated_probe_and_a_rebound_source() {
    // The cache is keyed on the container's NODE HANDLE and the warm-up builds
    // on the SECOND sight, so a single probe sees neither. This drives four
    // outer elements over one container (the index is built once and queried
    // three more times) and then the same arena over a DIFFERENT document,
    // where a stale index would answer with the first document's positions.
    const KEYS: &[&str] = &["1", "2", "1", "2"];
    for rows in [
        &[("1", "10"), ("2", "11"), ("1", "12")] as &[(&str, &str)],
        &[("2", "20"), ("1", "21"), ("2", "22")],
    ] {
        let fixture = JoinFixture { rows, keys: KEYS };
        let mut naive_resources = resources();
        let naive = drive_join(&fixture, false, &mut naive_resources);
        let mut indexed_resources = resources();
        let indexed = drive_join(&fixture, true, &mut indexed_resources);
        assert_eq!(indexed, naive);
    }
}

#[test]
fn a_refused_memory_grant_declines_to_the_naive_scan() {
    // Hazard 12, and the vertical's hardest law: the index is an optimization
    // the ledger may refuse, and a refusal must never fail a program the naive
    // scan could run. The ceiling is swept from "everything fits" down to
    // "not even the request account fits"; at EVERY ceiling that completes at
    // all the emitted sequence must be the naive sequence exactly.
    let fixture = JoinFixture {
        rows: &[("1", "0"), ("2", "1"), ("1", "2"), ("3", "3"), ("1", "4")],
        keys: &["1", "2", "3"],
    };
    let mut naive_resources = resources();
    let expected = drive_join(&fixture, false, &mut naive_resources);
    assert!(!expected.is_empty(), "the fixture must emit something");

    let mut completed = 0_u32;
    let mut refused = 0_u32;
    for ceiling in [
        u64::MAX,
        1 << 20,
        1 << 16,
        1 << 14,
        1 << 13,
        1 << 12,
        1 << 9,
        // The refusal arm needs a ceiling below the request account's own
        // box, so the sweep exercises the account failing to construct.
        // The box shrinks whenever dead ledger fields leave it (272 bytes
        // down to under 256 when the reservation cells were cut), so pin
        // the floor well under any plausible box size instead of one byte
        // below the current one.
        1 << 6,
    ] {
        let Ok(account) = RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, ceiling, u64::MAX, u32::MAX))
        else {
            // Not even the request account fits: no route can run.
            refused += 1;
            continue;
        };
        let mut capped =
            ResourceContext::new(account, &CONTROL, WorkMeter::try_new_v1(4_096).expect("work")).expect("resources");
        let nodes = join_arena();
        let table = correlated_scans(&nodes);
        let Ok(input) = build_join_input(&fixture, &capped) else {
            // Not even the DOCUMENT fits: no route ran, so there is no claim
            // to make about one.
            refused += 1;
            continue;
        };
        let mut stream =
            EngineRunStream::seed(&nodes, nid(11), nid(11), 0, 2, &table, &[], &[], input, None).expect("test seed");
        let mut emitted = Vec::new();
        let mut ok = true;
        loop {
            match stream.poll(&mut capped) {
                Ok(RunPoll::Item(item)) => emitted.push(integer_of(&item)),
                Ok(RunPoll::Complete) => break,
                Ok(RunPoll::Pending) => panic!("ample credits; no pause expected"),
                Err(_) => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            completed += 1;
            assert_eq!(
                emitted, expected,
                "ceiling {ceiling} completed with a sequence the naive scan does not produce"
            );
        } else {
            refused += 1;
        }
    }
    assert!(
        completed >= 2 && refused >= 1,
        "the sweep must both complete and be refused, or it proves nothing \
             (completed={completed} refused={refused})"
    );
}

/// The recognizer differential: every top-k-shaped program, run over
/// every fixture through BOTH the bounded-heap path (the real partial-sort
/// table) and the forced floor (an empty table), must publish byte-identical
/// items and complete identically. The floor is the plain `sort`/`sort_by`
/// evaluation — the topk table is the ONLY difference between the two runs,
/// so a divergence is a bug in the recognizer or the `top_k` law, never in
/// the program. Every program must ENGAGE the row on the routed arm (the
/// table is non-empty), or the lane proves nothing.
/// The differential's routed-vs-floor witness: every row byte-identical
/// over every fixture, with the routed arm PROVEN engaged. The
/// folded-bound spellings (`.[0:(1+2)]`, `1 as $k | .[0:$k]`) only route
/// because the fold made their bounds authored literals.
#[allow(
    clippy::too_many_lines,
    reason = "one fixture×program matrix per recognizer row: the row list is the shape table \
                  the recognizer is read against, and splitting it would hide the totality the \
                  engine differential exists to prove"
)]
#[test]
fn topk_recognizer_routed_vs_forced_floor_is_byte_identical() {
    use crate::analysis::partial_sorts;

    let mut res = resources();

    let programs = [
        "sort | .[0:3]",
        "sort | .[:3]",
        "sort | .[0:2.7]",
        "sort | .[0:3]?",
        "sort | .[-2:]",
        "sort | .[-3.7:]",
        "sort_by(.x) | .[0:2]",
        "sort_by(.x, .y) | .[0:2]",
        "sort_by(.x) | .[-2:]",
        "[sort | .[0:3]] | length",
        // Later rows: the reversed sort (k-largest, descending)
        // and the single-element index reads (`first`/`last` lower to the
        // same Index steps).
        "sort_by(.x) | reverse | .[0:2]",
        "sort_by(.x) | reverse | .[0:3]",
        "sort_by(.x, .y) | reverse | .[0:2]",
        "sort | reverse | .[0:2]",
        "sort | .[0]",
        "sort | .[-1]",
        "sort | first",
        "sort | last",
        // The REVERSED index rows (plain sort only): position 0 of the
        // reversed stable sort is `.[-1]` of the sorted one, position -1 is
        // `.[0]` — same element, same tie law, same empty/null law.
        "sort | reverse | .[0]",
        "sort | reverse | .[-1]",
        // Computed-but-constant bounds reach the row only
        // because the fold made them authored literals. A decline here is a
        // missed recognition, a wrong route a byte difference.
        "sort | .[0:(1+2)]",
        "sort | .[:(1+2)]",
        "sort | .[(1-1):2]",
        "1 as $k | sort | .[0:$k]",
        // The sort-slice INSIDE a builtin filter argument: limit's argument
        // is a FlatMap over a call, so it is not inline-eligible and drives
        // on a POOLED nested machine — the one argument drive whose seed
        // must thread the parent's top-k table (`take_arg_machine`). The
        // recognizer already names the row inside the argument (the walk is
        // whole-arena); without the threading this spelling full-sorted.
        "limit(3; sort_by(.x) | .[0:2])",
    ];

    // Fixtures built as owned values (exact decimals where the bound law
    // turns on them). Arrays carry equal keys to exercise the tie law in
    // BOTH directions; non-arrays exercise the `sort_by` fallback (the
    // two-arrays / not-iterable floor arms).
    let mut fixture_objects = Vec::new();
    let make_objects = |pairs: &[(&str, &str)]| {
        let mut array = Array::try_new().expect("array");
        for (key, id) in pairs {
            let mut object = ObjectBuilder::try_with_capacity(2).expect("builder");
            object
                .try_insert_last(
                    ObjectKey::try_from_str("x").expect("key"),
                    Value::Number(Number::try_json_literal(key).expect("number")),
                )
                .expect("insert x");
            object
                .try_insert_last(
                    ObjectKey::try_from_str("id").expect("key"),
                    Value::Number(Number::try_json_literal(id).expect("number")),
                )
                .expect("insert id");
            array
                .try_push(Value::Object(object.try_finish().expect("finish")))
                .expect("push");
        }
        Value::Array(array)
    };
    let mut fixtures: Vec<(&str, Value)> = vec![
        (
            "ties",
            make_objects(&[("2", "0"), ("1", "1"), ("2", "2"), ("1", "3"), ("3", "4")]),
        ),
        (
            "distinct",
            make_objects(&[("5", "0"), ("3", "1"), ("9", "2"), ("1", "3"), ("7", "4")]),
        ),
        ("empty", Value::Array(Array::try_new().expect("array"))),
        (
            "fractional",
            make_objects(&[("5", "0"), ("3", "1"), ("1", "2"), ("4", "3"), ("2", "4")]),
        ),
        ("object", {
            let mut object = ObjectBuilder::try_with_capacity(1).expect("builder");
            object
                .try_insert_last(
                    ObjectKey::try_from_str("a").expect("key"),
                    Value::Number(Number::try_json_literal("1").expect("number")),
                )
                .expect("insert");
            Value::Object(object.try_finish().expect("finish"))
        }),
        ("scalar", Value::Number(Number::try_json_literal("5").expect("number"))),
        ("null", Value::Null),
        // The ALL-equal-keys extreme of the tie law: every element shares
        // one key, so the tail/reversed rows' later-index-wins law and the
        // head rows' earlier-index-wins law are the WHOLE answer.
        ("all_equal", make_objects_all_equal()),
        // A NON-CORE-tagged array: the untag path through
        // `take_partial_sort` / `eval_partial_sort` / `keyed_child` must
        // publish exactly what the untagged twin publishes.
        (
            "tagged",
            Value::try_tagged(
                jqf_data::TagId::try_new_unaccounted("!list").expect("tag"),
                make_objects(&[("5", "0"), ("3", "1"), ("9", "2"), ("1", "3"), ("7", "4")]),
            )
            .expect("tagged"),
        ),
    ];
    fixture_objects.append(&mut fixtures);

    for program in programs {
        let compiled = compiled_for_execute(program, &res);
        let arena = compiled.arena();
        let table = partial_sorts(arena);
        assert!(
            !table.is_empty(),
            "the differential's routed arm must engage a row for {program}"
        );
        for (name, fixture) in &fixture_objects {
            let outcome = CodecInputOutcome::Result(EngineResult::owned(fixture.clone()));
            let routed = run_to_items(&compiled, None, outcome, &mut res).expect("routed run");
            let outcome = CodecInputOutcome::Result(EngineResult::owned(fixture.clone()));
            let floor = run_to_items(&compiled, Some(&[]), outcome, &mut res).expect("floor run");
            assert_eq!(
                routed, floor,
                "routed != floor for {program} over {name}: a recognizer or top_k bug"
            );
        }
    }
}

/// The partial-sort route's RUNTIME engagement, pinned by counter delta the
/// way [`JOIN_ENGAGEMENTS`] is read: both the direct spelling (the parent
/// machine's own table) and the spelling INSIDE a builtin filter argument
/// (the pooled nested machine, whose seed threads the parent's table) must
/// take the bounded-heap path. Byte identity alone cannot prove engagement —
/// the floor publishes the same bytes.
#[test]
fn the_partial_sort_route_engages_direct_and_inside_an_argument() {
    use crate::analysis::partial_sorts;
    use core::sync::atomic::Ordering;

    let mut res = resources();
    for program in [
        "sort_by(.x) | .[0:2]",
        // The argument drive: limit's argument is a FlatMap over a call, so
        // it seeds a POOLED machine — the drive whose empty-table seed this
        // pin guards. A regression back to `&[]` full-sorts silently; only
        // this counter notices.
        "limit(3; sort_by(.x) | .[0:2])",
    ] {
        let compiled = compiled_for_execute(program, &res);
        let table = partial_sorts(compiled.arena());
        assert!(!table.is_empty(), "{program} must name a row");
        let before = super::PARTIAL_SORT_ENGAGEMENTS.load(Ordering::Relaxed);
        let fixture = make_objects_all_equal();
        let outcome = CodecInputOutcome::Result(EngineResult::owned(fixture));
        let _ = run_to_items(&compiled, None, outcome, &mut res).expect("routed run");
        let after = super::PARTIAL_SORT_ENGAGEMENTS.load(Ordering::Relaxed);
        assert!(
            after > before,
            "{program} must ENGAGE the bounded-heap route (delta {})",
            after - before
        );
    }
}

/// The reversed row's TIE law, pinned outright: `reverse` of a stable sort
/// flips the tie order, so among equal keys the LATER source elements must
/// come FIRST in the published head. The LARGEST heap keeps the same set
/// the tail slice would (later indices among equal keys) and the reversed
/// emission orders them descending — `[id 4, id 3]` for an all-equal-key
/// five-element array, exactly `sort_by(.x) | reverse | .[0:2]`.
#[test]
fn the_reversed_row_emits_ties_in_reversed_source_order() {
    use crate::analysis::partial_sorts;

    let mut res = resources();
    let all_equal = make_objects_all_equal();
    for program in ["sort_by(.x) | reverse | .[0:2]", "sort_by(.x) | reverse | .[0:10]"] {
        let compiled = compiled_for_execute(program, &res);
        let table = partial_sorts(compiled.arena());
        assert!(!table.is_empty(), "{program} must engage a row");
        let outcome = CodecInputOutcome::Result(EngineResult::owned(all_equal.clone()));
        let routed = run_to_items(&compiled, None, outcome, &mut res).expect("routed");
        let outcome = CodecInputOutcome::Result(EngineResult::owned(all_equal.clone()));
        let floor = run_to_items(&compiled, Some(&[]), outcome, &mut res).expect("floor");
        assert_eq!(routed, floor, "routed != floor for {program} over all-equal ties");
        // The reversed-stable order, pinned by index: the two LARGEST
        // elements are the last two SOURCE elements, published descending.
        let expected = if program.ends_with("[0:2]") {
            "[{\"x\":1,\"id\":4},{\"x\":1,\"id\":3}]\n"
        } else {
            // k >= n: the whole reversed sort, ties descending.
            "[{\"x\":1,\"id\":4},{\"x\":1,\"id\":3},{\"x\":1,\"id\":2},{\"x\":1,\"id\":1},{\"x\":1,\"id\":0}]\n"
        };
        assert_eq!(routed, expected, "{program}: the reversed tie order");
    }
}

/// The index rows' empty-array law: `sort | .[0]` and `sort | .[-1]` over
/// an EMPTY array publish `null` (the index step's answer), never `[]` —
/// the k=1 heap is empty and the row shapes the empty result to `null`.
#[test]
fn an_index_row_over_an_empty_array_publishes_null() {
    use crate::analysis::partial_sorts;

    let mut res = resources();
    let empty = Value::Array(Array::try_new().expect("array"));
    for program in ["sort | .[0]", "sort | .[-1]", "sort | first", "sort | last"] {
        let compiled = compiled_for_execute(program, &res);
        let table = partial_sorts(compiled.arena());
        assert!(!table.is_empty(), "{program} must engage a row");
        let outcome = CodecInputOutcome::Result(EngineResult::owned(empty.clone()));
        let routed = run_to_items(&compiled, None, outcome, &mut res).expect("routed");
        let outcome = CodecInputOutcome::Result(EngineResult::owned(empty.clone()));
        let floor = run_to_items(&compiled, Some(&[]), outcome, &mut res).expect("floor");
        assert_eq!(routed, floor, "routed != floor for {program} over []");
        assert_eq!(routed, "null\n", "{program} over [] must publish null, not []");
    }
}

/// A five-element all-equal-key object array (`{x: 1, id: i}`) — the
/// reversed row's tie-law fixture.
fn make_objects_all_equal() -> Value {
    let mut array = Array::try_new().expect("array");
    for id in 0_i64..5 {
        let mut object = ObjectBuilder::try_with_capacity(2).expect("builder");
        object
            .try_insert_last(
                ObjectKey::try_from_str("x").expect("key"),
                Value::Number(Number::try_json_literal("1").expect("number")),
            )
            .expect("insert x");
        object
            .try_insert_last(
                ObjectKey::try_from_str("id").expect("key"),
                Value::Number(Number::try_json_literal(&format!("{id}")).expect("number")),
            )
            .expect("insert id");
        array
            .try_push(Value::Object(object.try_finish().expect("finish")))
            .expect("push");
    }
    Value::Array(array)
}

#[test]
fn slice_const_fold_routed_vs_floor_is_byte_identical() {
    // A folded computed slice bound must publish EXACTLY what
    // the authored literal would. The routed arm is the compiled program as
    // lowered (the fold already inside); the floor twin is the same program
    // re-lowered with the computed bound forced dynamic via a `bind_operand`
    // shape that cannot fold (an input read). The two must agree byte for
    // byte over arrays (empty and not), strings, null, objects, and the
    // negative-fold case.

    let mut res = resources();

    let twins = [
        (".[(1+2):]", ".[3:]"),
        (".catalog[(10-2):(1+2)]", ".catalog[8:3]"),
        ("1 as $x | .[$x:]", "1 as $x | .[1:]"),
        ("2 as $x | 1 as $y | .[$y:$x]", "2 as $x | 1 as $y | .[1:2]"),
        // A DECLINED computed bound must still equal its own authored
        // outcome: the fold did not run, so the runtime frame computes
        // `1*2 = 2` and the program is byte-identical to `.[2:]`.
        (".[(1*2):]", ".[2:]"),
    ];

    let fixtures: [(&str, Value); 6] = [
        ("array", Value::Array(array_of(&["0", "1", "2", "3", "4"]))),
        ("empty", Value::Array(Array::try_new().expect("array"))),
        ("string", Value::try_string("aébç").expect("string")),
        ("null", Value::Null),
        ("object", {
            let mut object = ObjectBuilder::try_with_capacity(1).expect("builder");
            object
                .try_insert_last(
                    ObjectKey::try_from_str("a").expect("key"),
                    Value::Number(Number::try_json_literal("1").expect("number")),
                )
                .expect("insert");
            Value::Object(object.try_finish().expect("finish"))
        }),
        ("big", Value::Array(array_of(&["5", "4", "3", "2", "1"]))),
    ];

    for (folded, authored) in twins {
        let folded_program = compiled_for_execute(folded, &res);
        let authored_program = compiled_for_execute(authored, &res);
        for (name, fixture) in &fixtures {
            let outcome = CodecInputOutcome::Result(EngineResult::owned(fixture.clone()));
            let folded_run = run_to_items(&folded_program, None, outcome, &mut res).expect("folded run");
            let outcome = CodecInputOutcome::Result(EngineResult::owned(fixture.clone()));
            let authored_run = run_to_items(&authored_program, None, outcome, &mut res).expect("authored run");
            assert_eq!(folded_run, authored_run, "folded != authored for {folded} over {name}");
        }
    }
}

/// Builds a small owned array of decimal integers from spellings.
fn array_of(spellings: &[&str]) -> Array {
    let mut array = Array::try_new().expect("array");
    for spelling in spellings {
        array
            .try_push(Value::Number(Number::try_json_literal(spelling).expect("number")))
            .expect("push");
    }
    array
}

/// Runs one compiled program over one input through the engine and records
/// every published item's compact JSON plus the completion, as a comparable
/// string. `None` is the production table on [`Program`]. `Some(&[])` is the
/// forced floor: the same program with an empty partial-sort table.
fn run_to_items(
    compiled: &crate::compile::CompiledProgram,
    topk: Option<&[PartialSort]>,
    outcome: CodecInputOutcome<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<String, EngineRunError> {
    let run = match topk {
        None => try_run_program(&compiled.program, compiled.program.split().prefix_len(), outcome, None),
        Some(topk) => try_run_with_table(&compiled.program, outcome, topk),
    }
    .map_err(EngineRunError::Codec)?;
    let EngineRun::Stream { mut stream, .. } = run else {
        return Ok(String::from("<non-stream>"));
    };
    let mut out = String::new();
    loop {
        match stream.poll(resources) {
            Ok(RunPoll::Item(EngineResult::Owned(value))) => {
                out.push_str(&jqf_builtins::semantics::render::to_json(&value)?);
                out.push('\n');
            }
            Ok(RunPoll::Item(EngineResult::Located(_))) => {
                return Err(super::internal_contract(
                    "top-k differential fixture must publish owned values",
                ));
            }
            Ok(RunPoll::Complete) => return Ok(out),
            Ok(RunPoll::Pending) => panic!("ample credits; no pause expected"),
            // A raise is part of the compared stream: the routed path must
            // surface the SAME error the floor does, byte for byte.
            Err(error) => {
                let _ = core::fmt::Write::write_fmt(&mut out, format_args!("<error:{error:?}>"));
                return Ok(out);
            }
        }
    }
}

#[test]
fn the_correlated_join_engages_on_the_real_spelling() {
    // The route's output is byte-identical to the naive scan's, so only the
    // engagement counter can tell the indexed route from a decline. The
    // compiled corpus spelling must engage: the warm-up first element takes
    // the naive path, the SECOND sight of the container builds the index and
    // every later element is answered from it.
    let mut resources = resources();
    let before = super::JOIN_ENGAGEMENTS.load(core::sync::atomic::Ordering::Relaxed);
    let program = compiled_for_execute(
        ".o as $o | [.u[0:3][] | . as $x | [$o[] | select(.k == $x.k)] | length]",
        &resources,
    );
    let fixture = JoinFixture {
        rows: &[("1", "0"), ("2", "1"), ("3", "2"), ("1", "3")],
        keys: &["1", "4", "1", "9", "2"],
    };
    let input = build_join_input(&fixture, &resources).expect("input");
    let run = program
        .execute(CodecInputOutcome::Result(input), &mut resources)
        .expect("run");
    let mut emitted = 0u64;
    match run {
        EngineRun::Stream { mut stream, .. } => loop {
            match stream.poll(&mut resources).expect("poll") {
                RunPoll::Item(_) => emitted += 1,
                RunPoll::Complete => break,
                RunPoll::Pending => panic!("ample credits; no pause expected"),
            }
        },
        _ => panic!("a resolved input must stream"),
    }
    let after = super::JOIN_ENGAGEMENTS.load(core::sync::atomic::Ordering::Relaxed);
    assert!(
        after > before,
        "the correlated join must ENGAGE on the real spelling (emitted {emitted} items)"
    );
}

// --- The user-declared reusable index --------------------------------
//
// `declare_index/2` is a TRANSPARENT declaration: its output is the input, so a
// probe-engaged run and a naive run publish identical bytes and only the
// engagement counter can tell them apart. The tests below assert (1) the
// declared index actually ENGAGES — a probe of the FIRST sight of the
// container, unlike the auto warm-up which goes naive once — (2) the
// engaged run is byte-identical to the forced `[.][0] |` floor, and
// (3) an assignment between the declaration and the probe DROPS the
// index (the edited document is owned, so the probe never consults it)
// with the naive scan's bytes unchanged.

#[test]
fn the_user_declared_index_engages_on_the_pipe_spelling() {
    let mut resources = resources();
    let before = super::USER_INDEX_PROBES.load(core::sync::atomic::Ordering::Relaxed);
    let program = compiled_for_execute("declare_index(.o; .k) | [.o[] | select(.k == 1)]", &resources);
    let fixture = JoinFixture {
        rows: &[("1", "0"), ("2", "1"), ("3", "2"), ("1", "3")],
        keys: &[],
    };
    let input = build_join_input(&fixture, &resources).expect("input");
    let run = program
        .execute(CodecInputOutcome::Result(input), &mut resources)
        .expect("run");
    match run {
        EngineRun::Stream { mut stream, .. } => loop {
            match stream.poll(&mut resources).expect("poll") {
                RunPoll::Item(_) => {}
                RunPoll::Complete => break,
                RunPoll::Pending => panic!("ample credits; no pause expected"),
            }
        },
        _ => panic!("a resolved input must stream"),
    }
    let after = super::USER_INDEX_PROBES.load(core::sync::atomic::Ordering::Relaxed);
    assert!(
        after > before,
        "the user-declared index must ENGAGE on the pipe spelling"
    );
}

#[test]
fn the_user_declared_index_is_byte_identical_to_the_forced_floor() {
    let mut resources = resources();
    let authored = compiled_for_execute("declare_index(.o; .k) | [.o[] | select(.k == 1)]", &resources);
    let floor = compiled_for_execute("[.][0] | declare_index(.o; .k) | [.o[] | select(.k == 1)]", &resources);
    let fixture = JoinFixture {
        rows: &[("1", "0"), ("2", "1"), ("3", "2"), ("1", "3")],
        keys: &[],
    };
    let authored_out = run_to_items(
        &authored,
        None,
        CodecInputOutcome::Result(build_join_input(&fixture, &resources).expect("input")),
        &mut resources,
    )
    .expect("authored run");
    let floor_out = run_to_items(
        &floor,
        None,
        CodecInputOutcome::Result(build_join_input(&fixture, &resources).expect("input")),
        &mut resources,
    )
    .expect("floor run");
    assert_eq!(
        authored_out, floor_out,
        "a declared index must never change bytes (authored {authored_out:?} vs floor {floor_out:?})"
    );
}

#[test]
fn the_user_declared_index_drops_after_an_edit() {
    let mut resources = resources();
    // The assignment between the declaration and the probe materializes
    // the document as an OWNED value, so the probe's container has no
    // node handle: the declared index is never consulted — dropped by
    // construction, never authoritative — and the naive scan answers.
    //
    // The witness is BYTES, not the engagement counter: the stale index
    // (built over keys {1,2,3}) would answer `[]` for key 9, where the
    // naive scan answers `[{"k":9,"i":0}]` — so byte identity with the
    // forced floor proves the index was dropped. (The counter is a global
    // static and Rust tests run in parallel, so an exact-equality
    // assertion on it would be a flake; the positive engagement test
    // asserts the monotone `moved` direction instead.)
    let program = compiled_for_execute(
        "declare_index(.o; .k) | (.o[0].k = 9) | [.o[] | select(.k == 9)]",
        &resources,
    );
    let floor = compiled_for_execute(
        "[.][0] | declare_index(.o; .k) | (.o[0].k = 9) | [.o[] | select(.k == 9)]",
        &resources,
    );
    let fixture = JoinFixture {
        rows: &[("1", "0"), ("2", "1"), ("3", "2"), ("1", "3")],
        keys: &[],
    };
    let authored_out = run_to_items(
        &program,
        None,
        CodecInputOutcome::Result(build_join_input(&fixture, &resources).expect("input")),
        &mut resources,
    )
    .expect("authored run");
    let floor_out = run_to_items(
        &floor,
        None,
        CodecInputOutcome::Result(build_join_input(&fixture, &resources).expect("input")),
        &mut resources,
    )
    .expect("floor run");
    assert_eq!(
        authored_out, floor_out,
        "an edit between declaration and probe must DROP the declared index"
    );
    assert!(
        authored_out.contains("{\"k\":9,\"i\":0}"),
        "the naive scan after the edit must see the edited value, not the stale index (got {authored_out:?})"
    );
}

/// A located markup ELEMENT array: root named `r`, children named
/// `b`, `a`, `b` with integer payloads 1, 2, 3. `.b` as a value selects
/// the first and third children; the document's real paths are `[0]` and
/// `[2]`.
fn located_markup_element(resources: &ResourceContext<'_>, children: &[(&str, &str)]) -> EngineResult<'static> {
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder");
    let mut items = Vec::new();
    for (name, value) in children {
        let node = builder
            .add_node("test.int", AccountedSemanticNode::Integer(value), None, resources)
            .expect("child");
        builder
            .add_fact(
                LocalOwnerRef::Node(node),
                "xml.name@1",
                "xml.name@1",
                1,
                &FactPayload::Text(String::from(*name)),
                resources,
            )
            .expect("child name");
        items.push(node);
    }
    let root = builder
        .add_node(
            "test.array",
            AccountedSemanticNode::Array { item_role: "test.item" },
            None,
            resources,
        )
        .expect("root");
    builder
        .add_fact(
            LocalOwnerRef::Node(root),
            "xml.name@1",
            "xml.name@1",
            1,
            &FactPayload::Text(String::from("r")),
            resources,
        )
        .expect("root name");
    for item in items {
        builder
            .add_occurrence(LocalOwnerRef::Node(root), "test.item", None, item, resources)
            .expect("edge");
    }
    let document = builder.finish(root, resources).expect("document");
    let product = DocumentProduct::try_new(document, resources).expect("product");
    let handle = product.document().root_handle();
    EngineResult::Located(LocatedProduct::try_new(&product, handle).expect("located"))
}

/// Compiles `program` and drives it over a located input, returning published
/// compact JSON lines or the first raise.
fn drive_located(
    program: &str,
    input: EngineResult<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<String, EngineRunError> {
    let compiled = compiled_for_execute(program, resources);
    let mut stream = match compiled
        .try_run_whole_value(CodecInputOutcome::Result(input), resources)
        .expect("run seeds")
    {
        EngineRun::Stream { stream, .. } => stream,
        EngineRun::Suppressed => return Ok(String::new()),
        EngineRun::Pushdown(error) => return Err(error),
        EngineRun::ReboundWhole => panic!("whole-value run must not rebound"),
    };
    let mut out = String::new();
    loop {
        match stream.poll(resources) {
            Ok(RunPoll::Item(EngineResult::Owned(value))) => {
                out.push_str(&jqf_builtins::semantics::render::to_json(&value)?);
                out.push('\n');
            }
            Ok(RunPoll::Item(EngineResult::Located(located))) => {
                let owned = located
                    .product()
                    .document()
                    .materialize_node(located.node(), resources)
                    .expect("located item materializes");
                out.push_str(&jqf_builtins::semantics::render::to_json(&owned)?);
                out.push('\n');
            }
            Ok(RunPoll::Complete) => return Ok(out),
            Ok(RunPoll::Pending) => {}
            Err(error) => return Err(error),
        }
    }
}

/// A markup member step `.name` that navigates as a value is usable in every
/// path position and resolves to the document's index paths.
#[test]
fn markup_member_step_is_path_navigable() {
    let mut resources = resources();
    let children = &[("b", "1"), ("a", "2"), ("b", "3")];

    let value = drive_located(".b", located_markup_element(&resources, children), &mut resources).expect("value walk");
    assert_eq!(value, "1\n3\n", "value walk selects both named children");

    let paths =
        drive_located("path(.b)", located_markup_element(&resources, children), &mut resources).expect("path walk");
    assert_eq!(
        paths, "[0]\n[2]\n",
        "path(.b) names the matched indices, not a string key"
    );

    let single_path = drive_located(
        "path(.b)",
        located_markup_element(&resources, &[("b", "1")]),
        &mut resources,
    )
    .expect("single path");
    assert_eq!(single_path, "[0]\n");

    let deleted = drive_located("del(.b)", located_markup_element(&resources, children), &mut resources).expect("del");
    assert_eq!(deleted, "[2]\n", "del(.b) removes both matched children");

    let got = drive_located(
        "getpath(path(.b))",
        located_markup_element(&resources, children),
        &mut resources,
    )
    .expect("getpath");
    assert_eq!(got, "1\n3\n", "getpath(path(.b)) reads the matched children");

    let updated = drive_located(
        ".b |= .+1",
        located_markup_element(&resources, children),
        &mut resources,
    )
    .expect("update");
    assert_eq!(updated, "[2,2,4]\n", ".b |= f updates each matched child");

    let named =
        drive_located(".@name", located_markup_element(&resources, children), &mut resources).expect("name accessor");
    assert_eq!(named, "\"r\"\n", ".@name stays the element-name fact");
}

/// A fact write whose value path navigates a MARKUP ELEMENT by key resolves
/// the child by its NAME FACT — the write twin of the member step above.
/// Before this law the Key arm demanded an object and raised
/// "Cannot index array with string", so `.b.&href = …` failed at run time on
/// exactly the documents where `.b.&href` READS fine. The read-back overlays
/// the delta, proving both halves of the write.
#[test]
fn markup_fact_write_navigates_children_by_name() {
    let mut resources = resources();

    let attribute = drive_located(
        r#".b.&href = "/y" | .b.&href"#,
        located_markup_element(&resources, &[("b", "1")]),
        &mut resources,
    )
    .expect("attribute write over a markup element");
    assert_eq!(attribute, "\"/y\"\n", "the two-step markup write lands");

    let comment = drive_located(
        ".b.@comment = [\"x\"] | .b.@comment",
        located_markup_element(&resources, &[("b", "1")]),
        &mut resources,
    )
    .expect("comment write over a markup element");
    assert_eq!(comment, "[\"x\"]\n", "the comment role writes and reads back");

    // Repeated sibling names: one fact write addresses ONE target, so a
    // plural match refuses loudly rather than publishing a silent partial
    // edit — `[n]` disambiguates. Recorded narrowing against the read side's
    // fan-out.
    let plural = drive_located(
        ".b.@comment = [\"x\"]",
        located_markup_element(&resources, &[("b", "1"), ("a", "2"), ("b", "3")]),
        &mut resources,
    );
    assert!(
        matches!(plural, Err(EngineRunError::Raised(_))),
        "a plural name match must refuse a single-target write"
    );

    // An unmatched name keeps the ordinary array-with-string mismatch, so a
    // plain JSON array's rejection is byte-identical to the pre-markup law.
    let miss = drive_located(
        ".z.@comment = [\"x\"]",
        located_markup_element(&resources, &[("b", "1")]),
        &mut resources,
    );
    let Err(EngineRunError::Raised(_)) = miss else {
        panic!("an unmatched markup name keeps the index-class mismatch");
    };
}

/// A located `{"p": "n"}` object document for the fact-write probes. for the fact-write probes.
fn located_p_object(resources: &ResourceContext<'_>) -> EngineResult<'static> {
    let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder");
    let child = builder
        .add_node("test.string", AccountedSemanticNode::String("n"), None, resources)
        .expect("member value");
    let root = builder
        .add_node(
            "test.object",
            AccountedSemanticNode::Object {
                member_role: "test.member",
            },
            None,
            resources,
        )
        .expect("root object");
    builder
        .add_occurrence(
            LocalOwnerRef::Node(root),
            "test.member",
            Some(AccountedOccurrenceKey::Text("p")),
            child,
            resources,
        )
        .expect("member p");
    let product =
        DocumentProduct::try_new(builder.finish(root, resources).expect("document"), resources).expect("product");
    EngineResult::Located(LocatedProduct::try_new(&product, product.document().root_handle()).expect("located root"))
}

/// A `@tag` WRITE is visible to a later `.@tag` READ in the same run: the
/// read consults the run's tag deltas before the node's intrinsic tag, the
/// same overlay every sibling role reads through. Without that consult the
/// write silently vanished (`.p.@tag = "!m" | .p.@tag` answered `null`).
#[test]
fn a_tag_write_overlays_a_later_tag_read() {
    let mut resources = resources();

    let static_form = drive_located(
        ".p.@tag = \"!m\" | .p.@tag",
        located_p_object(&resources),
        &mut resources,
    )
    .expect("static tag round trip");
    assert_eq!(static_form, "\"!m\"\n");

    let dynamic_form = drive_located(
        r#""tag" as $t | .p.@($t) = "!m" | .p.@tag"#,
        located_p_object(&resources),
        &mut resources,
    )
    .expect("dynamic tag round trip");
    assert_eq!(dynamic_form, "\"!m\"\n");
}

/// JSON object keys stay string path components; a plain array still rejects
/// a string key. Markup wiring must not change either law.
#[test]
fn json_path_keys_are_unchanged_by_markup_member_wiring() {
    let mut resources = resources();
    let object = {
        let mut builder = ObjectBuilder::try_with_capacity(1).expect("builder");
        builder
            .try_insert_last(ObjectKey::try_from_str("b").expect("key"), int_value("1"))
            .expect("insert");
        Value::Object(builder.try_finish().expect("object"))
    };
    let paths = drive_source("path(.b)", object, &mut resources).expect("json path");
    assert_eq!(paths, "[\"b\"]\n");

    let array = owned_int_array(&["1", "2"], &resources);
    let error = drive_source("path(.b)", array, &mut resources).expect_err("array+string");
    assert!(
        matches!(
            error,
            EngineRunError::TypeMismatch {
                actual_type: ValueKind::Array,
                ..
            }
        ),
        "plain arrays keep the string-key mismatch, got {error:?}"
    );
}

/// A reduce whose update is identity still addresses the register root: the
/// fold rematerializes the located input, and that twin must share the
/// register allocation (Law 1). A fresh owned root would raise
/// `Invalid path expression with result`.
#[test]
fn path_of_reduce_identity_update_is_the_empty_path() {
    let mut resources = resources();
    let input = located_value(&TestValue::Obj(&[("a", TestValue::Int("1"))]), &resources);
    let out = drive_located("path(reduce .a as $x (.; .))", input, &mut resources).expect("identity reduce tracks");
    assert_eq!(out, "[]\n");
}

/// Empty-path assignment over a located document: `getpath([])` rematerializes
/// the root, and that twin must still be the register address so the write
/// replaces the document.
#[test]
fn empty_path_assign_replaces_the_located_root() {
    let mut resources = resources();
    let input = located_value(&TestValue::Obj(&[("a", TestValue::Int("1"))]), &resources);
    let out = drive_located("getpath([]) = 5", input, &mut resources).expect("empty-path assign");
    assert_eq!(out, "5\n");
}

/// A miss on located null (`as [$v]`) leaves the register on owned null.
/// Law 1 still tracks the original located null by value, so the first
/// `?//` arm succeeds and `path` publishes `[0,"a"]`. The last arm is not
/// entered; under strict, the body's `.a` stays inside the chain's `try`
/// and `field-on-null` does not escape.
#[test]
fn path_of_null_alternative_destructure_takes_the_first_arm() {
    const PROGRAM: &str = "path(. as [$v] ?// {a: $v} | .a)";
    let mut lenient = resources();
    let out = drive_located(PROGRAM, located_null(&lenient), &mut lenient).expect("first arm tracks located null");
    assert_eq!(out, "[0,\"a\"]\n");

    let mut strict = resources().with_mismatch_policy(MismatchPolicy::Strict);
    let out = drive_located(PROGRAM, located_null(&strict), &mut strict)
        .expect("strict keeps the first arm inside the chain try");
    assert_eq!(out, "[0,\"a\"]\n");
}

/// A constructed object that equals the located root is still not the
/// register address: Law 1 is allocation identity, not value equality.
#[test]
fn constructed_object_equal_to_the_located_root_does_not_track() {
    let mut resources = resources();
    let input = located_value(&TestValue::Obj(&[("a", TestValue::Int("1"))]), &resources);
    let error = drive_located("path({a:1})", input, &mut resources).expect_err("constructed twin must not track");
    assert!(
        matches!(error, EngineRunError::Raised(_)),
        "constructed equal object must fail Law 3, got {error:?}"
    );
}

/// Pipeline stages are classified when the walk opens, not per child.
#[test]
fn path_walk_stages_are_resolved_once_per_walk() {
    let mut resources = resources();
    let input = owned_int_array(&["1", "2"], &resources);
    let out = drive_source("[paths(scalars)]", input, &mut resources).expect("paths(scalars)");
    assert_eq!(out, "[[0],[1]]\n");
}

/// Kind and truth come from one view. `paths(scalars)` still omits
/// `false`/`null` (select's truth law) and keeps other scalars.
#[test]
fn path_walk_derives_child_kind_once() {
    let mut resources = resources();
    let input = Value::Array(
        jqf_data::Array::try_from_vec(vec![Value::Bool(true), Value::Bool(false), Value::Null, int_value("1")])
            .expect("array"),
    );
    let out = drive_source("[paths(scalars)]", input, &mut resources).expect("paths(scalars)");
    assert_eq!(out, "[[0],[3]]\n");
}

/// A non-kind body reseeds one machine per child. `paths(.id)` still names
/// every object's id path.
#[test]
fn path_walk_reuses_one_stage_machine() {
    let mut resources = resources();
    let mut builder = ObjectBuilder::try_with_capacity(1).expect("builder");
    builder
        .try_insert_last(ObjectKey::try_from_str("id").expect("key"), int_value("1"))
        .expect("insert");
    let one = Value::Object(builder.try_finish_unique().expect("object"));
    let mut builder = ObjectBuilder::try_with_capacity(1).expect("builder");
    builder
        .try_insert_last(ObjectKey::try_from_str("id").expect("key"), int_value("2"))
        .expect("insert");
    let two = Value::Object(builder.try_finish_unique().expect("object"));
    let input = Value::Array(jqf_data::Array::try_from_vec(vec![one, two]).expect("array"));
    let out = drive_source("[paths(select(type == \"number\"))]", input, &mut resources).expect("paths(select type)");
    assert_eq!(out, "[[0,\"id\"],[1,\"id\"]]\n");
}

/// Emission consumers take the fields the walk already matched. Binary,
/// conditional, alternative, logical, and select must still answer.
#[test]
fn emission_consumers_reuse_the_matched_route() {
    let mut resources = resources();
    let input = int_value("2");
    assert_eq!(
        drive_source(". + 3", input.clone(), &mut resources).expect("binary"),
        "5\n"
    );
    assert_eq!(
        drive_source("if . then 1 else 0 end", input.clone(), &mut resources).expect("conditional"),
        "1\n"
    );
    assert_eq!(
        drive_source("null // .", input.clone(), &mut resources).expect("alternative"),
        "2\n"
    );
    assert_eq!(
        drive_source(". and true", input.clone(), &mut resources).expect("logical"),
        "true\n"
    );
    assert_eq!(
        drive_source("select(. > 1)", input, &mut resources).expect("select"),
        "2\n"
    );
}

/// Walk and `map_values` rebuild objects from the source object's own keys
/// at unique cursor positions. Those keys are already unique, so the
/// rebuild never mints a duplicate.
#[test]
fn object_rebuild_never_mints_duplicate_keys() {
    let mut resources = resources();
    let mut builder = ObjectBuilder::try_with_capacity(20).expect("builder");
    for i in 0..20 {
        builder
            .try_insert_last(
                ObjectKey::try_from_str(&format!("k{i:02}")).expect("key"),
                int_value(&format!("{i}")),
            )
            .expect("insert");
    }
    let source = Value::Object(builder.try_finish_unique().expect("unique source"));
    let source_keys = object_keys(&source);

    let walked = drive_source_value("walk(.)", source.clone(), &mut resources).expect("walk");
    let walk_keys = object_keys(&walked);
    assert_keys_unique(&walk_keys);
    assert_eq!(
        walk_keys, source_keys,
        "identity walk keeps source keys in first-insertion order"
    );

    let mapped = drive_source_value("map_values(.)", source.clone(), &mut resources).expect("map_values");
    let mapped_keys = object_keys(&mapped);
    assert_keys_unique(&mapped_keys);
    assert_eq!(
        mapped_keys, source_keys,
        "identity map_values keeps source keys in first-insertion order"
    );

    let deleted =
        drive_source_value("map_values(select(. % 2 == 1))", source, &mut resources).expect("map_values deletion");
    let deleted_keys = object_keys(&deleted);
    assert_keys_unique(&deleted_keys);
    assert_eq!(
        deleted_keys.len(),
        10,
        "odd-value members remain after even-value deletion"
    );
    for key in &deleted_keys {
        assert!(
            source_keys.iter().any(|source| source == key),
            "rebuild invented a key {key:?} the source did not have"
        );
    }
}

/// Insertion-order keys of an owned object.
fn object_keys(value: &Value) -> Vec<String> {
    let Value::Object(object) = value.untagged() else {
        panic!("expected object, got {value:?}");
    };
    object.iter().map(|entry| String::from(entry.key())).collect()
}

fn assert_keys_unique(keys: &[String]) {
    for (i, key) in keys.iter().enumerate() {
        for later in &keys[i + 1..] {
            assert_ne!(key, later, "rebuild minted a duplicate key {key:?}");
        }
    }
}

/// Drives a compiled program over an owned input and returns its single result.
fn drive_source_value(
    program: &str,
    input: Value,
    resources: &mut ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let compiled = compiled_for_execute(program, resources);
    let mut stream = match compiled
        .try_run_whole_value(CodecInputOutcome::Result(EngineResult::owned(input)), resources)
        .expect("run seeds")
    {
        EngineRun::Stream { stream, .. } => stream,
        EngineRun::Suppressed => {
            return Err(super::internal_contract("drive_source_value produced no stream"));
        }
        EngineRun::Pushdown(error) => return Err(error),
        EngineRun::ReboundWhole => panic!("whole-value run must not rebound"),
    };
    let mut out = None;
    loop {
        match stream.poll(resources) {
            Ok(RunPoll::Item(EngineResult::Owned(value))) => {
                assert!(out.is_none(), "drive_source_value expected one result, got a second");
                out = Some(value);
            }
            Ok(RunPoll::Item(EngineResult::Located(_))) => {
                return Err(super::internal_contract("owned-input drive publishes located"));
            }
            Ok(RunPoll::Complete) => {
                return out.ok_or_else(|| super::internal_contract("drive_source_value produced no item"));
            }
            Ok(RunPoll::Pending) => {}
            Err(error) => return Err(error),
        }
    }
}

/// `drive_source` that replenishes the 4096-credit slice so a 20k-element
/// all/reduce does not busy-loop on Pending.
fn drive_replenished(
    program: &str,
    input: Value,
    resources: &mut ResourceContext<'_>,
) -> Result<String, EngineRunError> {
    let compiled = compiled_for_execute(program, resources);
    let mut stream = match compiled
        .try_run_whole_value(CodecInputOutcome::Result(EngineResult::owned(input)), resources)
        .expect("run seeds")
    {
        EngineRun::Stream { stream, .. } => stream,
        EngineRun::Suppressed => return Ok(String::new()),
        EngineRun::Pushdown(error) => return Err(error),
        EngineRun::ReboundWhole => panic!("whole-value run must not rebound"),
    };
    let mut out = String::new();
    loop {
        match stream.poll(resources) {
            Ok(RunPoll::Item(EngineResult::Owned(value))) => {
                out.push_str(&jqf_builtins::semantics::render::to_json(&value)?);
                out.push('\n');
            }
            Ok(RunPoll::Item(EngineResult::Located(_))) => {
                return Err(super::internal_contract("owned-input drive publishes located"));
            }
            Ok(RunPoll::Complete) => return Ok(out),
            Ok(RunPoll::Pending) => {
                resources.try_begin_next_cooperative_entry(4_096).expect("entry");
            }
            Err(error) => return Err(error),
        }
    }
}

/// The `any`/`all` prelude still expands through `isempty`/`first`/`label`,
/// and over a large array the expansion answers exactly what the direct
/// iterate and reduce spellings answer.
#[test]
fn any_all_expansion_answers_like_the_reduce_spelling() {
    use crate::program::ProgramNode;

    let mut resources = resources();
    let expanded = compiled_for_execute("all(. > 0)", &resources);
    let mut labels = 0usize;
    let mut breaks = 0usize;
    let mut logicals = 0usize;
    for node in expanded.arena() {
        match node {
            ProgramNode::Label { .. } => labels += 1,
            ProgramNode::Break { .. } => breaks += 1,
            ProgramNode::Logical { .. } => logicals += 1,
            _ => {}
        }
    }
    assert!(
        labels >= 1 && breaks >= 1 && logicals >= 1,
        "all(. > 0) must still be the isempty/first/label expansion; \
         got labels={labels} breaks={breaks} logicals={logicals}"
    );

    let items: Vec<Value> = (0..100_000usize).map(|_| int_value("1")).collect();
    let input = Value::Array(Array::try_from_vec(items).expect("array"));

    assert_eq!(
        drive_replenished("[.[] | . > 0] | length", input.clone(), &mut resources).expect("iterate"),
        "100000\n",
        "the iterate spelling counts every element"
    );
    assert_eq!(
        drive_replenished("all(. > 0)", input.clone(), &mut resources).expect("expanded"),
        "true\n",
        "the expansion answers the same boolean the fold does"
    );
    assert_eq!(
        drive_replenished("reduce .[] as $x (true; . and ($x > 0))", input.clone(), &mut resources).expect("reduce"),
        "true\n",
        "the reduce spelling computes the same boolean"
    );
}

/// `GraphFrame` stride audit. The frame stays wide — past the small-enum
/// band and fatter than its walk and keyed-collect components, so boxing
/// either component alone cannot shrink the stride — and a bound result is
/// never smaller than the owned value it embeds.
#[test]
fn graph_frame_variant_sizes() {
    extern crate std;
    use super::{GraphFrame, GraphMachine};
    use crate::exec::path_register::PathWalk;
    use core::mem::size_of;
    use jqf_builtins::semantics::keyed::KeyValue;

    let frame = size_of::<GraphFrame>();
    let machine = size_of::<GraphMachine>();
    let walk = size_of::<PathWalk>();
    let key_value = size_of::<KeyValue>();
    let value = size_of::<Value>();
    let engine_result = size_of::<EngineResult>();
    std::eprintln!(
        "GraphFrame={frame} GraphMachine={machine} PathWalk={walk} \
         KeyValue={key_value} Value={value} EngineResult={engine_result}"
    );
    assert!(frame > 64, "GraphFrame should stay the wide frame enum, got {frame}");
    // A component is worth boxing only when it IS the fat slot. Neither
    // the path-walk state nor the keyed-collect pair is — both sit under
    // the whole-frame stride.
    assert!(
        frame > walk,
        "GraphFrame ({frame}) is fatter than PathWalk ({walk}); walk/collect are not the unique fat variants"
    );
    assert!(
        frame > key_value,
        "GraphFrame ({frame}) is fatter than KeyValue ({key_value}); the collect pair is not the fat slot"
    );
    // A bound result embeds its owned payload (`EngineResult::Owned(Value)`),
    // so the env slot can never be smaller than the value it carries.
    assert!(
        engine_result >= value,
        "EngineResult ({engine_result}) must embed Value ({value})"
    );
}

/// The opt-in iteration ceiling (`--max-iterations`) stops an unbounded
/// generator grind at the cooperative admissions: `[repeat(1)] | length` —
/// the collect fused into the repeat frame — refuses with the machine
/// resource family (the depth limits' channel) instead of running forever.
#[test]
fn iteration_cap_refuses_an_unbounded_repeat() {
    use jqf_resource::ResourceError;

    let mut resources = resources();
    let compiled = compiled_for_execute("[repeat(1)] | length", &resources);
    let outcome = CodecInputOutcome::Result(EngineResult::owned(Value::Null));
    let EngineRun::Stream { mut stream, .. } = compiled
        .try_run_whole_value(outcome, &resources)
        .expect("run")
        .with_iteration_cap(Some(1_000_000))
    else {
        panic!("expected a stream run");
    };
    let error = loop {
        match stream.poll(&mut resources) {
            Ok(RunPoll::Pending) => {
                // Refill the cooperative slice the way the CLI host does, or
                // the run would pend forever without ever advancing.
                resources.try_begin_next_cooperative_entry(4_096).expect("slice");
            }
            Ok(RunPoll::Item(_)) => {}
            Ok(RunPoll::Complete) => panic!("an unbounded repeat must not complete"),
            Err(error) => break error,
        }
    };
    let EngineRunError::Codec(failure) = &error else {
        panic!("expected a machine failure, got {error:?}");
    };
    assert!(
        matches!(
            failure.kind(),
            jqf_codec_core::CodecFailureKind::Resource(ResourceError::HostFailure { .. })
        ),
        "got {failure:?}"
    );
    assert_eq!(error.diagnostic_code(), jqf_resource::diag::codes::MACHINE_RESOURCE);
}

/// A generous ceiling leaves a bounded run byte-for-byte itself: the counter
/// ticks beside the admissions and nothing else changes.
#[test]
fn iteration_cap_leaves_a_bounded_run_untouched() {
    let mut resources = resources();
    let compiled = compiled_for_execute("[.[]] | length", &resources);
    let mut array = jqf_data::Array::try_new().expect("array");
    for text in ["1", "2", "3"] {
        array
            .try_push(Value::Number(Number::try_json_literal(text).expect("number")))
            .expect("push");
    }
    let outcome = CodecInputOutcome::Result(EngineResult::owned(Value::Array(array)));
    let EngineRun::Stream { mut stream, .. } = compiled
        .try_run_whole_value(outcome, &resources)
        .expect("run")
        .with_iteration_cap(Some(1_000_000))
    else {
        panic!("expected a stream run");
    };
    let mut polled = None;
    loop {
        match stream.poll(&mut resources) {
            Ok(RunPoll::Item(item)) => polled = Some(item),
            Ok(RunPoll::Pending) => {
                // Refill the cooperative slice the way the CLI host does.
                resources.try_begin_next_cooperative_entry(4_096).expect("slice");
            }
            Ok(RunPoll::Complete) => break,
            Err(error) => panic!("a bounded run under a generous cap must not refuse: {error:?}"),
        }
    }
    let Some(item) = polled else { panic!("no item") };
    let EngineResult::Owned(value) = item else {
        panic!("expected an owned count");
    };
    assert!(matches!(&value, Value::Number(n) if n.to_i64() == Some(3)));
}

/// A `reduce` with a multi-output INIT starts one complete fold per init
/// value, so an object member holding one is NOT a proven single
/// combination: the member's second output reused the moved-out partial and
/// published `{b:23}` without `a`. Both objects stay whole. (The law runs
/// every fold over the OUTER dot, so both folds answer; the clobber-on-the-
/// second-init shape is the catalogued dot-clobber intentional difference.)
#[test]
fn reduce_multi_output_init_keeps_object_members() {
    let mut resources = resources();
    let input = owned_int_array(&["1", "2"], &resources);
    let out = drive_source(
        "{a: 7, b: (reduce .[] as $x ((10, 20); . + $x))}",
        input,
        &mut resources,
    )
    .expect("both folds");
    assert_eq!(out, "{\"a\":7,\"b\":13}\n{\"a\":7,\"b\":23}\n");
}

/// A raise that escapes every barrier tears the run down while the
/// alternative's suppression region is still live: the wholesale frame
/// discard must run the region exits, or the depth leaks on the shared
/// request context and the NEXT run on that context swallows its own strict
/// mismatch.
#[test]
fn tear_down_exits_live_suppression_regions() {
    let mut resources = resources().with_mismatch_policy(MismatchPolicy::Strict);
    let leak = drive_source("(error(\"boom\")) // 1", int_value("1"), &mut resources)
        .expect_err("a left raise ends the alternative");
    assert!(matches!(leak, EngineRunError::Raised(_)), "got {leak:?}");

    // The SAME context: a leaked depth would suppress this miss.
    let mut builder = ObjectBuilder::try_with_capacity(1).expect("builder");
    builder
        .try_insert_last(ObjectKey::try_from_str("a").expect("key"), int_value("1"))
        .expect("insert");
    let error = drive_source(
        ".missing",
        Value::Object(builder.try_finish().expect("finish")),
        &mut resources,
    )
    .expect_err("strict mismatch must raise after the teardown");
    assert!(matches!(error, EngineRunError::MismatchRaised { .. }), "got {error:?}");
}

/// `select` over a multi-output predicate consumes TWO outputs on ONE
/// `SelectBody` frame: the frozen scope snapshot must survive past the
/// first output, or the SECOND predicate arm drives unfrozen, its `.b`
/// navigation extends the register, and the second emission fails Law 3
/// against it instead of publishing `[]`. Both arms answer two empty
/// paths.
#[test]
fn select_multi_output_predicate_under_path_raises() {
    // Recorded narrowing against jq (which answers two empty paths): the
    // select consumer's frozen scope ends per output, so a second truthy
    // output routes with the register moved and is refused. Single-output
    // predicates — every corpus shape — are unaffected. Revisit only with a
    // redesign that can hold the freeze across outputs without making the
    // register refuse publications.
    let mut resources = resources();
    let mut builder = ObjectBuilder::try_with_capacity(2).expect("builder");
    builder
        .try_insert_last(ObjectKey::try_from_str("a").expect("key"), int_value("1"))
        .expect("insert");
    builder
        .try_insert_last(ObjectKey::try_from_str("b").expect("key"), int_value("2"))
        .expect("insert");
    let doc = Value::Object(builder.try_finish().expect("finish"));
    let error =
        run_render("path(select(.a, .b))", &doc, &mut resources).expect_err("the second arm's publication is refused");
    assert!(matches!(error, EngineRunError::Raised(_)), "got {error:?}");
}

/// A tail self-call whose VALUE argument produces ZERO combinations
/// (`f(empty)` against a `$x` parameter) emits nothing: the seeded
/// empty-combos frame pops without binding parameters and without re-seeding
/// the body, exactly as the nested drive does. Re-evaluating the body over
/// unbound parameters would spin to the round limit; clean completion is the
/// law here. (A FILTER-parameter spelling — plain `x` — keeps jq's lazy
/// closure semantics, under which this program diverges and real jq aborts,
/// so it pins nothing here.)
#[test]
fn zero_combination_tail_call_emits_nothing() {
    let mut resources = resources();
    let out = drive_source("def f($x): f(empty); f(1)", int_value("1"), &mut resources)
        .expect("zero-combination call completes");
    assert_eq!(out, "");
}

// --- `equal_key_run`'s too-deep law ---

/// A single-element array chain `depth` levels deep wrapping one null —
/// deep enough that ordering two such chains against each other crosses the
/// comparison ceiling.
fn wrapped(depth: usize) -> Value {
    let mut value = Value::Null;
    for _ in 0..depth {
        let mut array = Array::try_new().expect("array");
        array.try_push(value).expect("push");
        value = Value::Array(array);
    }
    value
}

fn join_entry(key: Value, position: u32) -> JoinEntry {
    JoinEntry { key, position }
}

fn text_key(text: &str, position: u32) -> JoinEntry {
    join_entry(Value::try_string(text).expect("string key"), position)
}

/// Runs a body that builds and compares near-ceiling-depth values on a
/// thread sized for it: the comparison and the value drop both recurse once
/// per level, and a test thread's default stack is a fraction of what the
/// engine's own request thread carries.
fn with_deep_value_stack<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
    extern crate std;
    std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(body)
        .expect("deep-value test thread")
        .join()
        .expect("deep-value test body")
}

#[test]
fn equal_key_run_declines_when_the_window_holds_a_too_deep_key() {
    // The probe is deep enough that a DEEPER key cannot be ordered against
    // it (the comparison trips its ceiling), while shallow keys order by
    // rank in one step. The reported window must not survive the recheck.
    let answer = with_deep_value_stack(|| {
        let probe = wrapped(10_100);
        let entries = vec![
            text_key("a", 0),
            text_key("a", 1),
            join_entry(wrapped(10_200), 2),
            join_entry(
                Value::Object(ObjectBuilder::new().try_finish().expect("empty object")),
                3,
            ),
        ];
        equal_key_run(&entries, &probe)
    });
    assert!(answer.is_none());
}

#[test]
fn equal_key_run_bisects_a_clean_window() {
    let probe = Value::try_string("mid").expect("string key");
    let entries = vec![
        text_key("a", 0),
        text_key("mid", 1),
        text_key("mid", 2),
        text_key("z", 3),
    ];
    assert_eq!(equal_key_run(&entries, &probe), Some((1, 3)));
}

#[test]
fn json_exact_collect_count_miss_rebounds_whole() {
    // JSON Exact republishes PATH as the document root. Collect probe decline
    // must not try_run_whole_value on that node: skip is 0, demand.path is
    // `.users`. ReboundWhole is the host's rebind.
    let mut resources = resources();
    let program = compiled_for_execute("[.users[].name] | length", &resources);
    assert!(
        matches!(program.shortcut(), crate::compile::Shortcut::Count(_)),
        "collect-count must be a count row"
    );
    let located = located_value(
        &TestValue::Arr(&[TestValue::Obj(&[("name", TestValue::Int("1"))]), TestValue::Int("5")]),
        &resources,
    );
    let run = program
        .execute(CodecInputOutcome::Result(located), &mut resources)
        .expect("execute");
    assert!(
        matches!(run, EngineRun::ReboundWhole),
        "JSON Exact collect miss must rebound, got {run:?}"
    );
}

fn compiled_for_execute(source: &str, resources: &ResourceContext<'_>) -> crate::compile::CompiledProgram {
    use crate::codec_requirement::CodecRequirementPolicy;
    use crate::compile::try_compile_program;
    use jqf_codec_core::{DiagnosticPolicy, ValidationMode};
    try_compile_program(
        source,
        CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly),
        CompileOptions::new(),
        resources,
    )
    .expect("compiles")
}

fn drive_execute(
    source: &str,
    outcome: CodecInputOutcome<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<String, EngineRunError> {
    let compiled = compiled_for_execute(source, resources);
    collect_execute(&compiled, outcome, resources)
}

fn collect_execute(
    compiled: &crate::compile::CompiledProgram,
    outcome: CodecInputOutcome<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<String, EngineRunError> {
    collect_from_run(compiled.execute(outcome, resources).expect("execute seeds"), resources)
}

fn collect_graph(
    compiled: &crate::compile::CompiledProgram,
    outcome: CodecInputOutcome<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<String, EngineRunError> {
    collect_from_run(
        compiled.try_run_whole_value(outcome, resources).expect("graph seeds"),
        resources,
    )
}

fn collect_from_run(run: EngineRun<'_, '_>, resources: &mut ResourceContext<'_>) -> Result<String, EngineRunError> {
    let mut stream = match run {
        EngineRun::Stream { stream, .. } => stream,
        EngineRun::Suppressed => return Ok(String::new()),
        EngineRun::Pushdown(error) => return Err(error),
        EngineRun::ReboundWhole => panic!("execute rebounded Whole"),
    };
    let mut out = String::new();
    loop {
        match stream.poll(resources) {
            Ok(RunPoll::Item(EngineResult::Owned(value))) => {
                out.push_str(&jqf_builtins::semantics::render::to_json(&value)?);
                out.push('\n');
            }
            Ok(RunPoll::Item(EngineResult::Located(located))) => {
                let owned = located
                    .product()
                    .document()
                    .materialize_node(located.node(), resources)
                    .expect("located item materializes");
                out.push_str(&jqf_builtins::semantics::render::to_json(&owned)?);
                out.push('\n');
            }
            Ok(RunPoll::Complete) => return Ok(out),
            Ok(RunPoll::Pending) => {}
            Err(error) => return Err(error),
        }
    }
}

fn owned_object(pairs: &[(&str, Value)]) -> Value {
    let mut builder = ObjectBuilder::try_with_capacity(pairs.len()).expect("builder");
    for (key, value) in pairs {
        builder
            .try_insert_or_replace(ObjectKey::try_from_str(key).expect("key"), value.clone())
            .expect("insert");
    }
    Value::Object(builder.try_finish().expect("object"))
}

fn located_child(value: &TestValue, keys: &[&str], resources: &ResourceContext<'_>) -> EngineResult<'static> {
    let EngineResult::Located(located) = located_value(value, resources) else {
        panic!("located_value builds a Located product");
    };
    let document = located.product().document();
    let mut view = document.value_view(located.node()).expect("root view");
    for key in keys {
        let object = view.object().ok().flatten().expect("object");
        view = object.get(key).expect("named child");
    }
    let handle = document.node_handle(view.node()).expect("child handle");
    EngineResult::Located(LocatedProduct::try_new(located.product(), handle).expect("named child"))
}

#[test]
fn alternative_hoist_is_byte_identical_to_the_piped_and_floor_spellings() {
    let mut resources = resources();
    let compiled_hoisted = compiled_for_execute(".a.b // .a.c", &resources);
    let compiled_piped = compiled_for_execute(".a | (.b // .c)", &resources);
    let compiled_floor = compiled_for_execute(". as $z | .a.b // .a.c", &resources);
    let input = owned_object(&[("a", owned_object(&[("b", int_value("1")), ("c", int_value("2"))]))]);
    let outcome = || CodecInputOutcome::Result(EngineResult::owned(input.clone()));
    let hoisted = collect_graph(&compiled_hoisted, outcome(), &mut resources).expect("hoisted");
    let piped = collect_graph(&compiled_piped, outcome(), &mut resources).expect("piped");
    let floor = collect_graph(&compiled_floor, outcome(), &mut resources).expect("floor");
    assert_eq!(hoisted, "1\n");
    assert_eq!(hoisted, piped);
    assert_eq!(hoisted, floor);
}

#[test]
fn json_exact_element_iterates_the_located_container() {
    use crate::compile::Access;
    let mut resources = resources();
    let compiled = compiled_for_execute(".users[] | .id", &resources);
    assert_eq!(compiled.access(), Access::Exact);
    let located = located_value(
        &TestValue::Arr(&[
            TestValue::Obj(&[("id", TestValue::Int("1"))]),
            TestValue::Obj(&[("id", TestValue::Int("2"))]),
        ]),
        &resources,
    );
    let out = collect_execute(&compiled, CodecInputOutcome::Result(located), &mut resources).expect("hit");
    assert_eq!(out, "1\n2\n");
}

#[test]
fn yaml_exact_element_names_the_child_in_the_full_graph() {
    use crate::compile::Access;
    let mut resources = resources();
    let compiled = compiled_for_execute(".users[] | .id", &resources);
    assert_eq!(compiled.access(), Access::Exact);
    let full = TestValue::Obj(&[
        ("pad", TestValue::Obj(&[("id", TestValue::Int("99"))])),
        (
            "users",
            TestValue::Arr(&[
                TestValue::Obj(&[("id", TestValue::Int("1"))]),
                TestValue::Obj(&[("id", TestValue::Int("2"))]),
            ]),
        ),
    ]);
    let located = located_child(&full, &["users"], &resources);
    let EngineResult::Located(product) = &located else {
        panic!("named child is Located");
    };
    assert_ne!(
        product.node(),
        product.product().document().root_handle(),
        "YAML/HTML Exact keeps the full graph and names the child"
    );
    let out = collect_execute(&compiled, CodecInputOutcome::Result(located), &mut resources).expect("hit");
    assert_eq!(out, "1\n2\n");
}

#[test]
fn json_exact_element_miss_rebounds_whole() {
    let mut resources = resources();
    let compiled = compiled_for_execute(".users[] | .id", &resources);
    assert!(compiled.may_rebind_whole(), "element Exact miss uses the packed job");
    let located = located_value(&TestValue::Arr(&[TestValue::Int("1"), TestValue::Int("2")]), &resources);
    let run = compiled
        .execute(CodecInputOutcome::Result(located), &mut resources)
        .expect("execute");
    assert!(
        matches!(run, EngineRun::ReboundWhole),
        "JSON Exact element miss must rebound, got {run:?}"
    );
}

#[test]
fn empty_prefix_element_stays_whole() {
    use crate::compile::Access;
    let mut resources = resources();
    let compiled = compiled_for_execute(".[] | .id", &resources);
    assert_eq!(compiled.access(), Access::Whole);
    assert!(!compiled.may_rebind_whole());
    let located = located_value(
        &TestValue::Arr(&[TestValue::Obj(&[("id", TestValue::Int("7"))])]),
        &resources,
    );
    let out = collect_execute(&compiled, CodecInputOutcome::Result(located), &mut resources).expect("hit");
    assert_eq!(out, "7\n");
}

#[test]
fn element_oracle_decline_matches_the_graph() {
    let mut resources = resources();
    let compiled = compiled_for_execute(".[] | .id", &resources);
    let spec = TestValue::Arr(&[TestValue::Int("1"), TestValue::Int("2")]);
    let oracle = collect_execute(
        &compiled,
        CodecInputOutcome::Result(located_value(&spec, &resources)),
        &mut resources,
    );
    let graph = collect_graph(
        &compiled,
        CodecInputOutcome::Result(located_value(&spec, &resources)),
        &mut resources,
    );
    assert!(oracle.is_err(), "probe over numbers declines");
    match (oracle, graph) {
        (Err(left), Err(right)) => assert_eq!(alloc::format!("{left:?}"), alloc::format!("{right:?}")),
        (left, right) => panic!("decline must match the graph, oracle={left:?} graph={right:?}"),
    }
}

#[test]
fn alternative_optional_prefix_fence_still_tries_the_right_arm() {
    use crate::compile::Access;
    let mut resources = resources();
    let compiled = compiled_for_execute(".a?.x // 1", &resources);
    assert_eq!(compiled.access(), Access::Whole);
    assert!(compiled.program.split().is_whole_document());
    let out = drive_execute(
        ".a?.x // 1",
        CodecInputOutcome::Result(EngineResult::owned(int_value("5"))),
        &mut resources,
    )
    .expect("right arm");
    let floor = drive_execute(
        ". as $z | .a?.x // 1",
        CodecInputOutcome::Result(EngineResult::owned(int_value("5"))),
        &mut resources,
    )
    .expect("floor");
    assert_eq!(out, "1\n");
    assert_eq!(out, floor);
}

#[test]
fn json_exact_alternative_hoist_uses_the_located_node() {
    use crate::compile::Access;
    let mut resources = resources();
    let compiled = compiled_for_execute(".a.b // .a.c", &resources);
    assert_eq!(compiled.access(), Access::Exact);
    let located = located_value(
        &TestValue::Obj(&[("b", TestValue::Int("1")), ("c", TestValue::Int("2"))]),
        &resources,
    );
    let out = collect_execute(&compiled, CodecInputOutcome::Result(located), &mut resources).expect("hit");
    assert_eq!(out, "1\n");
}

#[test]
fn yaml_exact_alternative_hoist_names_the_child() {
    use crate::compile::Access;
    let mut resources = resources();
    let compiled = compiled_for_execute(".a.b // .a.c", &resources);
    assert_eq!(compiled.access(), Access::Exact);
    let full = TestValue::Obj(&[
        (
            "a",
            TestValue::Obj(&[("b", TestValue::Int("1")), ("c", TestValue::Int("2"))]),
        ),
        ("pad", TestValue::Int("9")),
    ]);
    let located = located_child(&full, &["a"], &resources);
    let EngineResult::Located(product) = &located else {
        panic!("named child is Located");
    };
    assert_ne!(product.node(), product.product().document().root_handle());
    let out = collect_execute(&compiled, CodecInputOutcome::Result(located), &mut resources).expect("hit");
    assert_eq!(out, "1\n");
}

#[test]
fn any_short_circuits_on_the_first_truthy_and_declines_a_raise_before_it() {
    use crate::compile::Shortcut;
    let mut resources = resources();
    let compiled = compiled_for_execute("any(.ok)", &resources);
    assert!(matches!(compiled.shortcut(), Shortcut::AnyAll(_)));
    let hit = TestValue::Arr(&[TestValue::Obj(&[("ok", TestValue::Bool(true))]), TestValue::Int("1")]);
    let out = collect_execute(
        &compiled,
        CodecInputOutcome::Result(located_value(&hit, &resources)),
        &mut resources,
    )
    .expect("short-circuit skips the later raise");
    assert_eq!(out, "true\n");

    let raise_first = TestValue::Arr(&[TestValue::Int("1"), TestValue::Obj(&[("ok", TestValue::Bool(true))])]);
    let oracle = collect_execute(
        &compiled,
        CodecInputOutcome::Result(located_value(&raise_first, &resources)),
        &mut resources,
    );
    let graph = collect_graph(
        &compiled,
        CodecInputOutcome::Result(located_value(&raise_first, &resources)),
        &mut resources,
    );
    assert!(oracle.is_err(), "raise before the first truthy is the graph's");
    match (oracle, graph) {
        (Err(left), Err(right)) => assert_eq!(alloc::format!("{left:?}"), alloc::format!("{right:?}")),
        (left, right) => panic!("raise-before-hit must match the graph, oracle={left:?} graph={right:?}"),
    }
}

#[test]
fn all_short_circuits_on_the_first_falsey_and_declines_a_raise_before_it() {
    use crate::compile::Shortcut;
    let mut resources = resources();
    let compiled = compiled_for_execute("all(.ok)", &resources);
    assert!(matches!(compiled.shortcut(), Shortcut::AnyAll(_)));
    let hit = TestValue::Arr(&[TestValue::Obj(&[("ok", TestValue::Bool(false))]), TestValue::Int("1")]);
    let out = collect_execute(
        &compiled,
        CodecInputOutcome::Result(located_value(&hit, &resources)),
        &mut resources,
    )
    .expect("short-circuit skips the later raise");
    assert_eq!(out, "false\n");

    let raise_first = TestValue::Arr(&[TestValue::Int("1"), TestValue::Obj(&[("ok", TestValue::Bool(false))])]);
    let oracle = collect_execute(
        &compiled,
        CodecInputOutcome::Result(located_value(&raise_first, &resources)),
        &mut resources,
    );
    let graph = collect_graph(
        &compiled,
        CodecInputOutcome::Result(located_value(&raise_first, &resources)),
        &mut resources,
    );
    assert!(oracle.is_err(), "raise before the first falsey is the graph's");
    match (oracle, graph) {
        (Err(left), Err(right)) => assert_eq!(alloc::format!("{left:?}"), alloc::format!("{right:?}")),
        (left, right) => panic!("raise-before-miss must match the graph, oracle={left:?} graph={right:?}"),
    }
}

#[test]
fn seed_nested_drops_join_tables_and_keeps_topk() {
    let resources = resources();
    let compiled = compiled_for_execute("limit(2; sort)", &resources);
    let input = EngineResult::owned(Value::Null);
    let parent = GraphMachine::from_program(
        &compiled.program,
        compiled.program.root(),
        compiled.program.split().entry(),
        0,
        input.try_clone().expect("clone"),
        true,
    )
    .expect("parent seed");
    assert!(parent.program.is_some());
    let nested = parent
        .seed_nested(compiled.program.root(), input, false)
        .expect("nested seed");
    assert!(nested.scans.is_empty(), "argument machines do not take join tables");
    assert!(nested.anti.is_empty(), "argument machines do not take anti tables");
    assert_eq!(
        parent.scans.len(),
        compiled.program.scans().len(),
        "parent join tables travel when requested"
    );
    assert_eq!(
        parent.anti.len(),
        compiled.program.anti_joins().len(),
        "parent anti tables travel when requested"
    );
    assert_eq!(nested.topk.len(), parent.topk.len(), "top-k is stateless and travels");
}

#[test]
fn production_graph_seed_holds_program() {
    // Residual production seed is &Program plus skip. A Choice root takes the
    // graph machine, which must hold Program so nested seeds inherit tables.
    let resources = resources();
    let compiled = compiled_for_execute("1, 2", &resources);
    let run = compiled
        .try_run_whole_value(CodecInputOutcome::Result(EngineResult::owned(Value::Null)), &resources)
        .expect("run");
    let EngineRun::Stream { stream, .. } = run else {
        panic!("comma residual is a stream, got {run:?}");
    };
    assert_eq!(
        stream.graph_holds_program(),
        Some(true),
        "production graph seed must hold Program"
    );
}
