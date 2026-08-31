//! Pre-fusion arena snapshots for the lowering surface.

use super::lower;
use crate::program::{ProgramNode, ProgramNodeId, StageStart, StepAccess, VarSlot};
use alloc::vec;
use alloc::vec::Vec;
use jqf_data::Value;
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};
use jqf_syntax::parse_query;

static CONTROL: ContinueControl = ContinueControl;

/// One unlimited compile-time ledger. Lowering never charges it — the
/// account exists because the module loader, the data-import decode, and
/// the allocation-refusal classification need a context even in a test.
fn test_resources() -> ResourceContext<'static> {
    let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
    let account = RequestAccount::try_new(limits).expect("test account");
    let work = WorkMeter::try_new_v1(1).expect("test work meter");
    ResourceContext::new(account, &CONTROL, work).expect("test ledger")
}

/// Runs the parse/bind/lower pipeline and returns the pre-fusion arena, so
/// these tests can observe the `FlatMap` nodes lowering constructs before
/// [`crate::analysis`] fuses them away.
fn lowered(source: &str) -> (Vec<ProgramNode>, ProgramNodeId) {
    let source_ref = SourceRef::new(SourceId::new(0), SourceKind::Query);
    let syntax = parse_query(source_ref, source)
        .expect("within input bound")
        .into_valid_syntax()
        .expect("valid syntax");
    let resolved = ResolvedSource::new(source_ref, "<program>", source.as_bytes(), 0);
    let bound = syntax.bind(resolved).expect("binds");
    let lowered = lower::try_lower_expr(bound.root(), bound.source(), &test_resources()).expect("lowers");
    let (nodes, root, ..) = lowered.into_program_parts();
    (nodes, root)
}

/// Split-lane lowering: enables unbound `$index` resolution.
fn split_lowered(source: &str) -> (Vec<ProgramNode>, ProgramNodeId, Option<u32>) {
    let source_ref = SourceRef::new(SourceId::new(0), SourceKind::Query);
    let syntax = parse_query(source_ref, source)
        .expect("within input bound")
        .into_valid_syntax()
        .expect("valid syntax");
    let resolved = ResolvedSource::new(source_ref, "<program>", source.as_bytes(), 0);
    let bound = syntax.bind(resolved).expect("binds");
    let lowered = lower::try_lower_expr_split(bound.root(), bound.source(), &test_resources()).expect("lowers");
    let (nodes, root, _, _, _, runtime_index_slot) = lowered.into_program_parts();
    (nodes, root, runtime_index_slot)
}

/// Asserts the node at `id` is a `Stage` and returns its `(key, optional)`
/// pairs — a compact witness for a step's access plus per-component flag.
fn stage_keys(nodes: &[ProgramNode], id: ProgramNodeId) -> Vec<(&str, bool)> {
    let ProgramNode::Stage { steps, .. } = &nodes[id.index()] else {
        panic!("node {id:?} is not a Stage");
    };
    steps
        .iter()
        .map(|step| match step.access() {
            StepAccess::Key(key) => (key.as_str(), step.is_optional()),
            StepAccess::Index(_)
            | StepAccess::Each
            | StepAccess::Descend
            | StepAccess::Slice(_)
            | StepAccess::DynVar(_)
            | StepAccess::NodeAccessor(_)
            | StepAccess::Attribute(_)
            | StepAccess::DynNodeAccessor(_)
            | StepAccess::DynAttribute(_) => {
                panic!("expected key access")
            }
        })
        .collect()
}

#[test]
fn pipe_lowers_to_flatmap_over_the_lowered_sides() {
    // Every pipe compile constructs a `FlatMap` in the arena; this observes
    // it before fusion consumes it.
    let (nodes, root) = lowered(".a | .b");
    let ProgramNode::FlatMap { upstream, body } = &nodes[root.index()] else {
        panic!("pipe root must be a FlatMap, got {:?}", nodes[root.index()]);
    };
    assert_eq!(stage_keys(&nodes, *upstream), [("a", false)]);
    assert_eq!(stage_keys(&nodes, *body), [("b", false)]);
}

#[test]
fn deep_pipe_chain_nests_flatmap_through_the_body() {
    // Pipe is right-associative, so `.a | .b | .c` nests the second pipe in
    // the outer `body`.
    let (nodes, root) = lowered(".a | .b | .c");
    let ProgramNode::FlatMap { upstream, body } = &nodes[root.index()] else {
        panic!("outer pipe must be a FlatMap");
    };
    assert_eq!(stage_keys(&nodes, *upstream), [("a", false)]);
    let ProgramNode::FlatMap {
        upstream: inner_upstream,
        body: inner_body,
    } = &nodes[body.index()]
    else {
        panic!("inner pipe must be a FlatMap");
    };
    assert_eq!(stage_keys(&nodes, *inner_upstream), [("b", false)]);
    assert_eq!(stage_keys(&nodes, *inner_body), [("c", false)]);
}

#[test]
fn optional_flag_rides_the_authoring_side_of_a_pipe() {
    // `.a? | .b` flags the upstream step; `.a | .b?` flags the body step. The
    // asymmetry is why the flag is per-step and travels with fusion.
    let (left_nodes, left_root) = lowered(".a? | .b");
    let ProgramNode::FlatMap { upstream, body } = &left_nodes[left_root.index()] else {
        panic!("pipe root must be a FlatMap");
    };
    assert_eq!(stage_keys(&left_nodes, *upstream), [("a", true)]);
    assert_eq!(stage_keys(&left_nodes, *body), [("b", false)]);

    let (right_nodes, right_root) = lowered(".a | .b?");
    let ProgramNode::FlatMap { upstream, body } = &right_nodes[right_root.index()] else {
        panic!("pipe root must be a FlatMap");
    };
    assert_eq!(stage_keys(&right_nodes, *upstream), [("a", false)]);
    assert_eq!(stage_keys(&right_nodes, *body), [("b", true)]);
}

#[test]
fn optional_flag_is_per_component_within_one_stage() {
    // `.a?.b` is a single stage (no pipe) whose first component is optional
    // and second is not — the exact-step law's in-stage form.
    let (nodes, root) = lowered(".a?.b");
    assert!(matches!(nodes[root.index()], ProgramNode::Stage { .. }));
    assert_eq!(stage_keys(&nodes, root), [("a", true), ("b", false)]);
}

/// Renders a stage's step `access`es as compact tokens so an `Each` step is
/// observable alongside keys.
fn stage_tokens(nodes: &[ProgramNode], id: ProgramNodeId) -> Vec<(alloc::string::String, bool)> {
    use alloc::string::ToString as _;
    let ProgramNode::Stage { steps, .. } = &nodes[id.index()] else {
        panic!("node {id:?} is not a Stage");
    };
    steps
        .iter()
        .map(|step| {
            let token = match step.access() {
                StepAccess::Key(key) => key.clone(),
                StepAccess::Index(index) => index.to_string(),
                StepAccess::Each => "[]".to_string(),
                StepAccess::Descend => "..".to_string(),
                StepAccess::Slice(bounds) => {
                    alloc::format!("{:?}:{:?}", bounds.start, bounds.end)
                }
                StepAccess::DynVar(slot) => alloc::format!("${slot}"),
                StepAccess::NodeAccessor(name) => alloc::format!("@{name}"),
                StepAccess::Attribute(name) => alloc::format!("&{name}"),
                StepAccess::DynNodeAccessor(slot) => alloc::format!("@(${slot})"),
                StepAccess::DynAttribute(slot) => alloc::format!("&(${slot})"),
            };
            (token, step.is_optional())
        })
        .collect()
}

#[test]
fn iteration_lowers_to_an_each_step_in_one_stage() {
    // `.a[].b` is one postfix chain (no pipe) → one Stage `[a, Each, b]`.
    let (nodes, root) = lowered(".a[].b");
    assert!(matches!(nodes[root.index()], ProgramNode::Stage { .. }));
    assert_eq!(
        stage_tokens(&nodes, root),
        [("a".into(), false), ("[]".into(), false), ("b".into(), false)]
    );
}

#[test]
fn iteration_optional_flag_rides_the_each_step() {
    // `.[]?` flags the `Each` step itself (exact-step law for the iterate).
    let (nodes, root) = lowered(".[]?");
    assert_eq!(stage_tokens(&nodes, root), [("[]".into(), true)]);
}

#[test]
fn and_or_lower_to_the_logical_family() {
    // `and`/`or` lower to a `Logical` node carrying the operator; `//` lowers to
    // an `Alternative`. All three are separate from the arithmetic `Binary`.
    use crate::program::LogicalOp;
    let (and_nodes, and_root) = lowered(". and .");
    assert!(matches!(
        and_nodes[and_root.index()],
        ProgramNode::Logical {
            operator: LogicalOp::And,
            ..
        }
    ));
    let (or_nodes, or_root) = lowered(". or .");
    assert!(matches!(
        or_nodes[or_root.index()],
        ProgramNode::Logical {
            operator: LogicalOp::Or,
            ..
        }
    ));
    let (alt_nodes, alt_root) = lowered(". // .");
    assert!(matches!(alt_nodes[alt_root.index()], ProgramNode::Alternative { .. }));
}

#[test]
fn if_without_else_synthesizes_an_identity_alternative() {
    // `if .a then 1 end`: the missing `else` synthesizes an identity stage as
    // the alternative (`if .a then 1 end` on `{"a":false}` → the input).
    let (nodes, root) = lowered("if .a then 1 end");
    let ProgramNode::Conditional { alternative, .. } = &nodes[root.index()] else {
        panic!("if lowers to a Conditional, got {:?}", nodes[root.index()]);
    };
    assert!(
        matches!(
            &nodes[alternative.index()],
            ProgramNode::Stage {
                start: StageStart::Current,
                steps,
            } if steps.is_empty()
        ),
        "a missing else is an identity stage alternative"
    );
}

#[test]
fn a_static_string_template_lowers_to_one_literal_stage() {
    // No holes: the template is one owned string, never a concat chain.
    for source in [r#""a""#, r#""""#, r#""a\tb""#] {
        let (nodes, root) = lowered(source);
        assert!(
            matches!(
                &nodes[root.index()],
                ProgramNode::Stage {
                    start: StageStart::Literal(Value::String(_)),
                    steps,
                } if steps.is_empty()
            ),
            "{source} must be a single string literal stage"
        );
    }
}

#[test]
fn an_interpolated_template_lowers_a_tostring_hole() {
    // A hole is `hole | tostring`; the concat around it is a later
    // lowering (today a left-associative `+` chain). The hole itself
    // must stay a real graph so a raise names the hole, not a
    // synthetic operator.
    let (nodes, root) = lowered(r#""x\(1)y""#);
    assert!(
        matches!(&nodes[root.index()], ProgramNode::Concat { parts } if parts.len() == 3),
        "a multi-part template must lower to one Concat, got {:?}",
        nodes[root.index()]
    );
    assert!(
        arena_has_tostring(&nodes, root),
        "an interpolated hole must lower through tostring"
    );
    let (nodes, root) = lowered(r#""\(.a.@tag)""#);
    assert!(
        arena_has_tostring(&nodes, root),
        r#""\(.a.@tag)" must compile a real hole graph"#
    );
    let (nodes, root) = lowered(r#""\(.a.&href)""#);
    assert!(
        arena_has_tostring(&nodes, root),
        r#""\(.a.&href)" must compile a real hole graph"#
    );
}

fn arena_has_tostring(nodes: &[ProgramNode], root: ProgramNodeId) -> bool {
    fn walk(nodes: &[ProgramNode], id: ProgramNodeId, seen: &mut [bool]) -> bool {
        let index = id.index();
        if index >= seen.len() || seen[index] {
            return false;
        }
        seen[index] = true;
        match &nodes[index] {
            ProgramNode::Call { overload, args, .. } => {
                jqf_builtins::registry::resolve_builtin("tostring", 0).is_some_and(|record| record.id == *overload)
                    || args.iter().any(|arg| walk(nodes, *arg, seen))
            }
            ProgramNode::FlatMap { upstream, body } => walk(nodes, *upstream, seen) || walk(nodes, *body, seen),
            ProgramNode::Binary { left, right, .. }
            | ProgramNode::Choice { left, right }
            | ProgramNode::Alternative { left, right }
            | ProgramNode::Logical { left, right, .. } => walk(nodes, *left, seen) || walk(nodes, *right, seen),
            ProgramNode::Concat { parts } => parts.iter().any(|part| walk(nodes, *part, seen)),
            ProgramNode::Stage { .. }
            | ProgramNode::Empty
            | ProgramNode::CallFilter { .. }
            | ProgramNode::Break { .. }
            | ProgramNode::EnginePull { .. } => false,
            ProgramNode::CollectArray { body } | ProgramNode::CountCollect { body } => {
                body.is_some_and(|body| walk(nodes, body, seen))
            }
            ProgramNode::ConstructObject { members } => members
                .iter()
                .any(|member| walk(nodes, member.key, seen) || walk(nodes, member.value, seen)),
            ProgramNode::CallDef {
                args,
                filter_args,
                body,
                ..
            } => {
                walk(nodes, *body, seen)
                    || args.iter().any(|arg| walk(nodes, *arg, seen))
                    || filter_args.iter().any(|arg| walk(nodes, *arg, seen))
            }
            ProgramNode::Conditional {
                condition,
                consequent,
                alternative,
            } => walk(nodes, *condition, seen) || walk(nodes, *consequent, seen) || walk(nodes, *alternative, seen),
            ProgramNode::Try { body, handler } => {
                walk(nodes, *body, seen) || handler.is_some_and(|handler| walk(nodes, handler, seen))
            }
            ProgramNode::ChainBody { body } | ProgramNode::Label { body, .. } => walk(nodes, *body, seen),
            ProgramNode::Bind { source, body, .. } | ProgramNode::EngineBind { source, body, .. } => {
                walk(nodes, *source, seen) || walk(nodes, *body, seen)
            }
            ProgramNode::Reduce {
                source, init, update, ..
            } => walk(nodes, *source, seen) || walk(nodes, *init, seen) || walk(nodes, *update, seen),
            ProgramNode::Foreach {
                source,
                init,
                update,
                extract,
                ..
            } => {
                walk(nodes, *source, seen)
                    || walk(nodes, *init, seen)
                    || walk(nodes, *update, seen)
                    || extract.is_some_and(|extract| walk(nodes, extract, seen))
            }
            ProgramNode::Counted { source, .. } => walk(nodes, *source, seen),
            ProgramNode::Modify { paths, update, .. } | ProgramNode::FactAssign { paths, update, .. } => {
                walk(nodes, *paths, seen) || walk(nodes, *update, seen)
            }
            ProgramNode::EngineGenerator { init, update, extract } => {
                walk(nodes, *init, seen) || walk(nodes, *update, seen) || walk(nodes, *extract, seen)
            }
            ProgramNode::EngineRng { seed } => walk(nodes, *seed, seen),
        }
    }
    walk(nodes, root, &mut vec![false; nodes.len()])
}

#[test]
fn a_variable_start_body_blocks_fusion_like_a_literal() {
    // A `Variable`-start stage ignores the upstream value, so it blocks
    // `Stage∘Stage` fusion exactly as a `Literal` start does. The
    // pipe survives as a `FlatMap` in the pre-fusion arena and the body keeps
    // its Variable start.
    let (nodes, root) = lowered(". as $x | (.a | $x)");
    let ProgramNode::Bind { body, .. } = &nodes[root.index()] else {
        panic!("a binding lowers to a Bind node");
    };
    let ProgramNode::FlatMap { body: inner, .. } = &nodes[body.index()] else {
        panic!("the piped body stays a FlatMap");
    };
    assert!(
        matches!(
            &nodes[inner.index()],
            ProgramNode::Stage {
                start: StageStart::Variable(0),
                ..
            }
        ),
        "the `$x` body is a Variable-start stage"
    );
}

#[test]
fn a_bound_variable_postfix_chain_is_one_stage() {
    // `$r.b[1]` composes its steps onto the ONE `Variable`-start stage, just
    // as `.b[1]` composes onto a `Current`-start one.
    let (nodes, root) = lowered(".a as $r | $r.b[1]");
    let ProgramNode::Bind { body, .. } = &nodes[root.index()] else {
        panic!("a binding lowers to a Bind node");
    };
    let ProgramNode::Stage {
        start: StageStart::Variable(0),
        steps,
    } = &nodes[body.index()]
    else {
        panic!("`$r.b[1]` is one Variable-start stage");
    };
    assert_eq!(steps.len(), 2, "the postfix steps fuse onto the base stage");
}

#[test]
fn a_dynamic_variable_index_lowers_to_one_dynvar_step() {
    // `.o[$x]` is ONE `DynVar` step carrying the resolved slot — the
    // runtime, not the lowerer, decides key-versus-index.
    let (nodes, root) = lowered(".k as $x | .o[$x]");
    let ProgramNode::Bind { body, .. } = &nodes[root.index()] else {
        panic!("a binding lowers to a Bind node");
    };
    assert_eq!(stage_tokens(&nodes, *body), [("o".into(), false), ("$0".into(), false)]);
}

#[test]
fn elif_chain_desugars_to_nested_conditionals() {
    // `if c1 then t1 elif c2 then t2 else e end` desugars to
    // `Conditional(c1, t1, Conditional(c2, t2, e))`: the outer alternative is
    // the inner Conditional.
    let (nodes, root) = lowered("if .a then 1 elif .b then 2 else 3 end");
    let ProgramNode::Conditional { alternative, .. } = &nodes[root.index()] else {
        panic!("outer if must be a Conditional");
    };
    assert!(
        matches!(&nodes[alternative.index()], ProgramNode::Conditional { .. }),
        "the elif branch is a nested Conditional in the outer alternative"
    );
}

#[test]
fn loc_binding_serves_operand_positions_through_the_expression_ladder() {
    // `.[$__loc__]` takes the ordinary operand frame any general
    // expression takes: the location literal is the Bind's source and the
    // chain is one DynVar step over the anonymous slot — the SAME shape
    // `.[$ENV]` lowers to, and no by-name refusal. The literal carries the
    // reference site's own location (line 1: the token sits on the first
    // line of the program text).
    let (nodes, root) = lowered(".[$__loc__]");
    let ProgramNode::Bind {
        source,
        slot,
        body,
        frame,
    } = &nodes[root.index()]
    else {
        panic!("an operand-position $__loc__ lowers through a Bind frame");
    };
    assert!(!frame, "the operand frame binds no user-visible name");
    let ProgramNode::Stage {
        start: StageStart::Literal(value),
        steps: literal_steps,
    } = &nodes[source.index()]
    else {
        panic!("the operand source must be the location literal");
    };
    assert!(literal_steps.is_empty());
    let jqf_data::Value::Object(object) = value.untagged() else {
        panic!("the operand source must be an object");
    };
    let jqf_data::Value::String(file) = object.get("file").map(jqf_data::Value::untagged).expect("file") else {
        panic!("file must be a string");
    };
    assert_eq!(file, "<program>");
    assert!(matches!(
        object.get("line").map(jqf_data::Value::untagged),
        Some(Value::Number(_))
    ));
    // The chain body reads the frame's own slot.
    let ProgramNode::Stage { steps, .. } = &nodes[body.index()] else {
        panic!("the indexed chain must be a stage");
    };
    assert_eq!(steps.len(), 1);
    assert!(matches!(
        steps[0].access(),
        StepAccess::DynVar(read) if *read == *slot
    ));
}

fn variable_slots(nodes: &[ProgramNode]) -> Vec<VarSlot> {
    nodes
        .iter()
        .filter_map(|node| match node {
            ProgramNode::Stage {
                start: StageStart::Variable(slot),
                ..
            } => Some(*slot),
            _ => None,
        })
        .collect()
}

#[test]
fn split_lane_reuses_one_index_slot_for_every_reference() {
    let (nodes, _, runtime_index_slot) = split_lowered(r#""\($index)-\($index)""#);
    let index_slot = runtime_index_slot.expect("split lane records the $index slot");
    let variable_stages = variable_slots(&nodes);
    assert_eq!(
        variable_stages.len(),
        2,
        "each $index hole lowers to one Variable stage"
    );
    assert!(
        variable_stages.iter().all(|slot| *slot == index_slot),
        "every $index reference must share the recorded runtime slot"
    );
}

#[test]
fn comma_lowers_to_choice_before_fusion() {
    let (nodes, root) = lowered(".a, .b");
    assert!(matches!(nodes[root.index()], ProgramNode::Choice { .. }));
}

#[test]
fn identity_pipe_folds_before_emitting_flatmap() {
    let (nodes, root) = lowered(". | .b");
    assert!(matches!(nodes[root.index()], ProgramNode::Stage { .. }));
    assert_eq!(stage_keys(&nodes, root), [("b", false)]);
}

#[test]
fn transparent_group_lowers_like_its_inner_expression() {
    let (plain, plain_root) = lowered(".a");
    let (grouped, grouped_root) = lowered("(.a)");
    assert_eq!(
        core::mem::discriminant(&plain[plain_root.index()]),
        core::mem::discriminant(&grouped[grouped_root.index()])
    );
    assert_eq!(stage_keys(&plain, plain_root), stage_keys(&grouped, grouped_root));
}

#[test]
fn array_length_without_each_fuses_to_count_collect() {
    let (nodes, root) = lowered("[.a] | length");
    assert!(matches!(nodes[root.index()], ProgramNode::CountCollect { .. }));
}

#[test]
fn array_length_with_each_stays_a_flatmap_pipe() {
    let (nodes, root) = lowered("[.[]] | length");
    assert!(matches!(nodes[root.index()], ProgramNode::FlatMap { .. }));
}

#[test]
fn nesting_depth_limit_is_enforced_at_lower_expr() {
    use jqf_resource::{RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
    let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, 2);
    let account = RequestAccount::try_new(limits).expect("account");
    let work = WorkMeter::try_new_v1(1).expect("work");
    let resources = ResourceContext::new(account, &CONTROL, work).expect("resources");
    let source_ref = SourceRef::new(SourceId::new(0), SourceKind::Query);
    let source = "(((1)))";
    let syntax = parse_query(source_ref, source)
        .expect("within input bound")
        .into_valid_syntax()
        .expect("valid syntax");
    let resolved = ResolvedSource::new(source_ref, "<program>", source.as_bytes(), 0);
    let bound = syntax.bind(resolved).expect("binds");
    let result = lower::try_lower_expr(bound.root(), bound.source(), &resources);
    assert!(matches!(
        result,
        Err(crate::compile::EngineCompileError::NestingTooDeep { .. })
    ));
}
