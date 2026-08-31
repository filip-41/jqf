//! Compile-pipeline snapshots: lowering, pushdown, prelude, and route facts.

use super::{
    CompileOptions, CompiledProgram, EngineCompileError, HostIo, Shortcut, UnsupportedConstruct, try_compile_program,
};
use crate::analysis::BoundaryConsumer;
use crate::codec_requirement::CodecRequirementPolicy;
use crate::program::{ProgramNode, SliceBound, StageStart, StageStep, StepAccess};
use alloc::{format, string::String, vec::Vec};
use jqf_builtins::registry::Evaluator;
use jqf_codec_core::{AccessRequirement, AccessResultKind, DiagnosticPolicy, PortableStep, ValidationMode};
use jqf_data::Value;
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

static CONTROL: ContinueControl = ContinueControl;

fn resources() -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(4096).expect("work"),
    )
    .expect("resources")
}

fn policy() -> CodecRequirementPolicy {
    CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly)
}

fn compile(source: &str) -> Result<CompiledProgram, EngineCompileError> {
    try_compile_program(source, policy(), CompileOptions::new(), &resources())
}

#[test]
fn slurp_aggregate_rewrite_recognizes_the_map_shapes() {
    use crate::program::ProgramNode;
    // add and length,  fused-stage and flatmap map bodies, and the explicit
    // `[.[] | F] | add` spelling (which lowers identically to `map(F)|add`).
    for src in [
        "map(.a)|add",
        "map(.a)|length",
        "map(select(.a>1))|add",
        "map(.[0])|length",
        "[.[]|.a]|add",
        "map(.a?)|add",
    ] {
        let rewritten = compiled(src)
            .streaming_slurp_aggregate()
            .unwrap_or_else(|| panic!("{src} must be recognized"));
        match &rewritten.arena()[rewritten.root().index()] {
            ProgramNode::Reduce { .. } => {}
            other => panic!("{src}: expected a Reduce root, got {other:?}"),
        }
    }
    // Every other shape declines: non-map pipes, arity-1 aggregates,
    // nothing after the aggregate, a bare map, an identity pipe.
    for src in [
        ".a",
        "map(.a)|add|length",
        "map(.a)",
        "add",
        "length",
        "map(.a)|.",
        ".[]|add",
    ] {
        assert!(
            compiled(src).streaming_slurp_aggregate().is_none(),
            "{src} must decline"
        );
    }
}

#[test]
fn collect_add_rewrites_to_reduce() {
    for src in [
        "[.catalog[] | .score] | add",
        ".catalog | map(.score) | add",
        "map(.score) | add",
    ] {
        let program = compiled(src);
        match &program.arena()[program.root().index()] {
            ProgramNode::Reduce { .. } => {}
            other => panic!("{src}: expected Reduce, got {other:?}"),
        }
    }
}

#[test]
fn constant_if_validates_discarded_then_arm() {
    assert!(matches!(
        compile("if null then $x else 1 end"),
        Err(EngineCompileError::UndefinedVariable { .. })
    ));
}

#[test]
fn constant_if_validates_discarded_else_arm() {
    assert!(matches!(
        compile("if true then 1 else $x end"),
        Err(EngineCompileError::UndefinedVariable { .. })
    ));
}

fn compiled(source: &str) -> CompiledProgram {
    compile(source).expect("compiles")
}

/// A stored Call carries the evaluator payload resolved at lower time.
#[test]
fn call_nodes_store_the_evaluator_payload() {
    assert!(
        compiled("length").arena().iter().any(|node| {
            matches!(
                node,
                ProgramNode::Call {
                    payload: Evaluator::Length,
                    ..
                }
            )
        }),
        "length must store Evaluator::Length on the Call node"
    );
}

fn compiled_with_args(source: &str, args: &[(String, Value)]) -> CompiledProgram {
    try_compile_program(
        source,
        policy(),
        CompileOptions {
            cli_vars: args,
            split_exp: false,
            source_label: "<top-level>",
        },
        &resources(),
    )
    .expect("compiles")
}

/// The loss-lane programs and the exact prune trees the walk must hand the
/// decode — the guard net over the fold, bind, and union shapes.
#[test]
fn prune_trees_over_the_loss_lane_shapes() {
    fn render(node: &crate::analysis::PruneNode, out: &mut String) {
        if node.is_all() {
            out.push('*');
            return;
        }
        out.push('{');
        if let Some(element) = node.element() {
            out.push_str("[]:");
            render(element, out);
        }
        for (key, child) in node.keys() {
            out.push(',');
            out.push_str(key);
            out.push(':');
            render(child, out);
        }
        out.push('}');
    }
    let lanes: &[(&str, Option<&str>)] = &[
        (
            "[[.users[].id], [.users[].name], [.users[].age]] | transpose | length",
            Some("{,users:{[]:{,age:*,id:*,name:*}}}"),
        ),
        (
            "[foreach .users[] as $u (0; . + 1; {n: ., id: $u.id})] | length",
            Some("{,users:{[]:{,id:*}}}"),
        ),
        (
            "[foreach .users[] as $u ({}; .[$u.tier] = true; length)] | last",
            Some("{,users:{[]:{,tier:*}}}"),
        ),
        (
            "reduce .events[] as $e ({}; .[$e.bucket] = ((.[$e.bucket] // 0) + 1))",
            Some("{,events:{[]:{,bucket:*}}}"),
        ),
        (
            "reduce .orders[] as [$user_id, $quantity] (0; . + $quantity)",
            Some("{,orders:{[]:{[]:*}}}"),
        ),
    ];
    for (source, expected) in lanes {
        let compiled = compile(source).expect("compiles");
        let (tree, _) = crate::analysis::prune_trees(
            compiled.program.nodes(),
            compiled.program.root(),
            compiled.program.slots(),
        );
        let rendered = tree.map(|tree| {
            let mut out = String::new();
            render(&tree, &mut out);
            out
        });
        assert_eq!(rendered.as_deref(), *expected, "{source}");
    }
}

#[test]
fn cli_args_bind_unbound_variables_to_literals() {
    let args = [
        (
            alloc::string::String::from("$name"),
            Value::try_string("hello").expect("string"),
        ),
        (
            alloc::string::String::from("$n"),
            Value::try_string("2").expect("string"),
        ),
    ];
    compiled_with_args("$name", &args);
    compiled_with_args("$name, $n", &args);
}

#[test]
fn cli_args_do_not_shadow_lexical_binders_or_env() {
    let args = [
        (
            alloc::string::String::from("$x"),
            Value::try_string("cli").expect("string"),
        ),
        (
            alloc::string::String::from("$ENV"),
            Value::try_string("cli").expect("string"),
        ),
    ];
    // A program binder wins: `1 as $x | $x` must be the binder, not the arg.
    compiled_with_args("1 as $x | $x", &args);
    // `$ENV` stays the environment read (`--arg ENV x` is shadowed by the
    // pre-bound environment binding).
    compiled_with_args("$ENV", &args);
    // A name bound by NEITHER the scopes nor the args is still the error.
    assert!(matches!(
        compile_with_args_err("$missing", &args),
        Err(EngineCompileError::UndefinedVariable { .. })
    ));
}

/// `{$ENV}`, `.[$ENV]` and `.[$ENV:]` compile. The three direct
/// `resolve_variable` sites raised where the lowered path had already
/// lowered `$ENV` to the `env/0` read; the reference accepts all three
/// spellings, and each now routes `$ENV` through the same
/// env/0 lowering — a member's value/key producer, an index operand
/// bound into a frame, and a slice bound. The runtime outcomes are
/// pinned by the corpus rows; this test pins the acceptance.
#[test]
fn env_variable_spellings_compile() {
    compiled("{$ENV}");
    compiled("{$ENV: 1}");
    compiled(".[$ENV]");
    compiled(".[$ENV:]");
    compiled(".[:$ENV]");
    // `$ENV` is always the pre-bound environment read, never an ordinary
    // scope slot — even `1 as $ENV | $ENV` stays env/0 (catalogue law).
    compiled("1 as $ENV | $ENV");
}

/// A CLI binding serves a variable in OPERAND position exactly as it
/// serves expression position: `.[$k]`, a slice bound, and a dynamic
/// accessor selector all lower through the general operand path under
/// `--arg`, while a genuinely unbound name still reports the ordinary
/// `$x is not defined` resolution error.
#[test]
fn cli_args_serve_operand_position_variables() {
    let args = [(
        alloc::string::String::from("$k"),
        Value::try_string("key").expect("string"),
    )];
    compiled_with_args(".[$k]", &args);
    compiled_with_args(".a[$k:] | length", &args);
    compiled_with_args(".[:$k]", &args);
    compiled_with_args(".@($k)", &args);
    compiled_with_args(".&($k)", &args);
    assert!(matches!(
        compile_with_args_err(".[$nope]", &args),
        Err(EngineCompileError::UndefinedVariable { .. })
    ));
}

/// The named bindings serve operand positions through the same expression
/// ladder as everywhere else: `$__loc__` compiles at an accessor and
/// raises its ordinary runtime mismatch there (`{file,line}` is not a
/// fact name), and `$ENV` lowers to the env/0 read.
#[test]
fn named_operand_bindings_keep_their_surface() {
    compiled(".[$__loc__]");
    compiled(".@($__loc__)");
    compiled(".[$ENV]");
    compiled(".@($ENV)");
    // The location literal itself is unchanged: the operand frame carries
    // exactly the value expression position produces.
    let compiled = compiled(".[$__loc__]");
    assert!(compiled.arena().iter().any(|node| matches!(
        node,
        crate::program::ProgramNode::Stage {
            start: crate::program::StageStart::Literal(_),
            ..
        }
    )));
}

/// A call with more arguments than the registry's one-byte arity ceiling
/// is rejected naming the AUTHORED count — never a clamped
/// `name/255 is not defined` reporting an arity the user never wrote.
#[test]
fn an_arity_past_the_ceiling_names_the_authored_count() {
    let mut source = alloc::string::String::from("f(");
    for i in 0..300 {
        if i > 0 {
            source.push_str("; ");
        }
        source.push('1');
    }
    source.push(')');
    match compile_with_args_err(&source, &[]) {
        Err(EngineCompileError::ArityLimit { count, .. }) => assert_eq!(count, 300),
        other => panic!("expected the arity-limit rejection, got {other:?}"),
    }
}

#[test]
fn cli_args_inside_defs_close_over_the_literal() {
    let args = [(
        alloc::string::String::from("$v"),
        Value::try_string("V").expect("string"),
    )];
    // `def f: $v; f` with `--arg v V` answers "V".
    compiled_with_args("def f: $v; f", &args);
}

#[test]
fn later_cli_args_shadow_earlier_ones() {
    let resources = resources();
    let args = [
        (
            alloc::string::String::from("$a"),
            Value::try_string("first").expect("string"),
        ),
        (
            alloc::string::String::from("$a"),
            Value::try_string("second").expect("string"),
        ),
    ];
    // The last-wins law: `--arg a 1 --arg a 2` answers "2".
    let program = try_compile_program(
        "$a",
        policy(),
        CompileOptions {
            cli_vars: &args,
            split_exp: false,
            source_label: "<top-level>",
        },
        &resources,
    )
    .expect("compiles");
    let nodes = program.program.nodes();
    let literals: Vec<_> = nodes
        .iter()
        .filter_map(|node| match node {
            ProgramNode::Stage {
                start: crate::program::StageStart::Literal(value),
                ..
            } => Some(match value.untagged() {
                jqf_data::Value::String(text) => text.as_str(),
                _ => "?",
            }),
            _ => None,
        })
        .collect();
    assert_eq!(literals, ["second"], "the later binding must win");
}

fn compile_with_args_err(source: &str, args: &[(String, Value)]) -> Result<CompiledProgram, EngineCompileError> {
    try_compile_program(
        source,
        policy(),
        CompileOptions {
            cli_vars: args,
            split_exp: false,
            source_label: "<top-level>",
        },
        &resources(),
    )
}

#[test]
fn host_io_echo_accepts_only_the_bare_identity_filter() {
    use crate::HostIo;
    assert_eq!(
        compiled(".").host_io(),
        HostIo::Echo,
        "bare `.` must be recognized as identity"
    );
    // `. | .` lowers through stage fusion to the same zero-step `Current`
    // stage, so it is provably identity too: it emits its input once,
    // unchanged, for every input.
    assert_eq!(
        compiled(". | .").host_io(),
        HostIo::Echo,
        "a fused identity pipe must be recognized as identity"
    );
    // The round-trip lane publishes the input bytes UNCHANGED, so anything
    // that could emit a different value or cardinality than its input —
    // every static path, iteration, recursion, constructor, call, and pipe
    // shape — must be rejected.
    for source in [
        ".a",
        ".a.b",
        ".a[0]",
        ".[]",
        "..",
        "[.]",
        ".a, .b",
        "(.a)",
        ".[] | .",
        "empty",
        "length",
        "input",
        "select(.)",
        "try .",
    ] {
        assert_ne!(
            compiled(source).host_io(),
            HostIo::Echo,
            "{source:?} must not be recognized as identity"
        );
    }
}

#[test]
fn modifies_answers_whether_the_program_assigns() {
    for source in [".", ".a", ".[]", "length", "select(.)"] {
        assert!(
            !compiled(source).modifies(),
            "{source:?} must not contain an assignment"
        );
    }
    for source in [".a = 1", ".a |= . + 1", "del(.a)", ".a += 1", "map_values(.)"] {
        assert!(compiled(source).modifies(), "{source:?} must contain an assignment");
    }
}

/// The steps of a program whose root is a single [`Stage`] (identity, static
/// path, iteration, or pipe fused to one stage) — the shape these tests
/// inspect. Panics for a graph root, which these tests never build.
///
/// [`Stage`]: ProgramNode::Stage
fn root_stage_steps(program: &CompiledProgram) -> &[StageStep] {
    match &program.program.nodes()[program.program.root().index()] {
        ProgramNode::Stage { steps, .. } => steps,
        other => panic!("expected a Stage root, got {other:?}"),
    }
}

fn keys(program: &CompiledProgram) -> Vec<&str> {
    root_stage_steps(program)
        .iter()
        .filter_map(|step| match step.access() {
            StepAccess::Key(key) => Some(key.as_str()),
            StepAccess::Index(_)
            | StepAccess::Each
            | StepAccess::Descend
            | StepAccess::Slice(_)
            | StepAccess::DynVar(_)
            | StepAccess::NodeAccessor(_)
            | StepAccess::Attribute(_)
            | StepAccess::DynNodeAccessor(_)
            | StepAccess::DynAttribute(_) => None,
        })
        .collect()
}

fn indices(program: &CompiledProgram) -> Vec<i64> {
    root_stage_steps(program)
        .iter()
        .filter_map(|step| match step.access() {
            StepAccess::Index(index) => Some(*index),
            StepAccess::Key(_)
            | StepAccess::Each
            | StepAccess::Descend
            | StepAccess::Slice(_)
            | StepAccess::DynVar(_)
            | StepAccess::NodeAccessor(_)
            | StepAccess::Attribute(_)
            | StepAccess::DynNodeAccessor(_)
            | StepAccess::DynAttribute(_) => None,
        })
        .collect()
}

/// The per-component optional (`?`) flags of the fused root stage, in order.
fn optional_flags(program: &CompiledProgram) -> Vec<bool> {
    root_stage_steps(program).iter().map(StageStep::is_optional).collect()
}

fn requirement_of(source: &str) -> AccessRequirement {
    let resources = resources();
    compiled(source).try_requirement(&resources).expect("requirement")
}

fn unsupported(source: &str) -> UnsupportedConstruct {
    match compile(source) {
        Err(EngineCompileError::Unsupported { construct, .. }) => construct,
        other => panic!("expected unsupported construct for {source:?}, got {other:?}"),
    }
}

/// The full rejection message as a user reads it.
fn unsupported_error(source: &str) -> EngineCompileError {
    match compile(source) {
        Err(error @ EngineCompileError::Unsupported { .. }) => error,
        other => panic!("expected an unsupported rejection for {source:?}, got {other:?}"),
    }
}

/// The rejection message as a user reads it, for the two honesty guards
/// below.
fn rejection_message() -> String {
    match compile("module (.+1); 0") {
        Err(error @ EngineCompileError::Unsupported { .. }) => alloc::format!("{error}"),
        other => panic!("expected an unsupported rejection, got {other:?}"),
    }
}

/// The STALE-MESSAGE TRIPWIRE for the builtin half.
///
/// The clause is generated from the registry, so this cannot fail while the
/// generation stands; what it pins is that the generation stands. Anyone who
/// replaces the generated clause with hand-written prose — which is how the
/// message went stale the first time, claiming four builtins when seven
/// resolved — fails here the moment the registry gains an overload the prose
/// does not mention.
#[test]
fn the_message_names_every_registered_builtin() {
    let message = rejection_message();
    assert!(!jqf_builtins::registry::builtin_overloads().is_empty());
    for overload in jqf_builtins::registry::builtin_overloads() {
        let spelling = alloc::format!("`{}/{}`", overload.canonical_name, overload.arity);
        assert!(
            message.contains(&spelling),
            "rejection message does not name the registered builtin {spelling}: {message}"
        );
    }
}

/// The mirror guard for the SYNTAX half: every form the message advertises
/// must actually compile.
///
/// The message's job is to tell a user what jqf runs. A claim that is no
/// longer true is the same defect as an omission, and the syntax clause has
/// no registry to generate it from — so each named form gets one probe
/// program here. Widening the grammar means adding the form to
/// `SUPPORTED_SYNTAX` and its probe to this list.
#[test]
fn the_message_names_only_forms_that_compile() {
    let probes = [
        (".", "identity `.`"),
        (".a", "field path"),
        (".\"k\"", "quoted field path"),
        (".[\"k\"]", "bracketed string key"),
        (".[0]", "index"),
        (".[-1]", "negative index"),
        (".a?", "per-component `?`"),
        (".[]", "`.[]` iteration"),
        ("..", "recursive descent `..`"),
        (".[1:2]", "literal-bound slice"),
        ("1 as $x | .[$x:]", "`$var`-bound slice"),
        (".[.a:.b]", "general-bound slice"),
        ("1 as $x | .[$x]", "dynamic indexing by `$var`"),
        (".[.k]", "dynamic indexing `.[e]`"),
        ("-.a", "unary negation `-`"),
        (".a | .b", "pipe `|`"),
        (".a, .b", "comma `,`"),
        ("(.a)", "parenthesized group"),
        ("1", "literal"),
        ("[.]", "`[…]` constructor"),
        ("{a: 1}", "`{…}` constructor"),
        (". + 1", "arithmetic `+`"),
        (". - 1", "arithmetic `-`"),
        (". * 1", "arithmetic `*`"),
        (". / 1", "arithmetic `/`"),
        (". % 1", "arithmetic `%`"),
        (". == 1", "comparison `==`"),
        (". != 1", "comparison `!=`"),
        (". < 1", "comparison `<`"),
        (". <= 1", "comparison `<=`"),
        (". > 1", "comparison `>`"),
        (". >= 1", "comparison `>=`"),
        (". and .", "`and`"),
        (". or .", "`or`"),
        (". // 1", "the alternative `//`"),
        ("if . then 1 else 2 end", "`if`/`else`"),
        ("if . then 1 elif . then 2 else 3 end", "`elif`"),
        ("try .a catch .", "`try`/`catch`"),
        ("(.a)?", "the `?` sugar"),
        (". as $x | $x", "`as` binding over a variable"),
        (". as [$a,$b] | $a", "`as [$a,$b]` array pattern"),
        (". as {a:$v,$b} | $v", "`as {a:$v,$b}` object pattern"),
        (". as [$a] ?// {$a} | $a", "the `?//` alternative chain"),
        ("reduce .[] as $x (0; . + $x)", "`reduce`"),
        ("foreach .[] as $x (0; . + $x)", "`foreach`"),
        ("label $out | break $out", "`label $out`/`break $out`"),
        (".a = 1", "assignment `=`"),
        (".a |= . + 1", "update `|=`"),
        (".a += 1", "the arithmetic update `+=`"),
        (".a -= 1", "the arithmetic update `-=`"),
        (".a *= 1", "the arithmetic update `*=`"),
        (".a /= 1", "the arithmetic update `/=`"),
        (".a %= 1", "the arithmetic update `%=`"),
        (".a //= 1", "the alternative update `//=`"),
    ];
    for (source, claim) in probes {
        assert!(
            compile(source).is_ok(),
            "the rejection message advertises {claim}, but {source:?} does not compile"
        );
    }
    // Every claim above must also be one the message actually makes; a
    // probe for a form the message stopped advertising is dead weight.
    let message = rejection_message();
    for fragment in [
        "identity `.`",
        "`.[]` iteration",
        "recursive descent `..`",
        "dynamic indexing `.[e]`",
        "unary negation `-`",
        "pipe `|`",
        "comma `,`",
        "arithmetic `+ - * / %`",
        "comparisons `== != < <= > >=`",
        "`and`/`or`",
        "the alternative `//`",
        "`if`/`elif`/`else`",
        "`try`/`catch`",
        "`as` bindings over a variable or a destructuring pattern",
        "its `?//` alternative chain",
        "`reduce`",
        "`foreach`",
        "assignment `=`",
        "update `|=`",
        "`+= -= *= /= %= //=`",
    ] {
        assert!(
            message.contains(fragment),
            "the supported-surface enumeration no longer states {fragment:?}: {message}"
        );
    }
}

#[test]
fn identity_is_whole_document_access() {
    let program = compiled(".");
    assert!(program.program.split().is_whole_document());
    assert!(root_stage_steps(&program).is_empty());
}

#[test]
fn identity_requirement_is_complete_document() {
    let resources = resources();
    let program = compile(".").expect("identity");
    let requirement = program.try_requirement(&resources).expect("requirement");
    assert_eq!(requirement.result(), AccessResultKind::CompleteDocument);
}

#[test]
fn single_field_path() {
    assert_eq!(keys(&compiled(".a")), ["a"]);
}

#[test]
fn nested_field_path() {
    assert_eq!(keys(&compiled(".a.b")), ["a", "b"]);
}

#[test]
fn quoted_field_key_with_spaces() {
    assert_eq!(keys(&compiled(r#"."quoted key""#)), ["quoted key"]);
}

#[test]
fn bracketed_string_key() {
    assert_eq!(keys(&compiled(r#".["a"]"#)), ["a"]);
}

#[test]
fn unicode_escape_key_decodes() {
    assert_eq!(keys(&compiled(r#"."café""#)), ["caf\u{e9}"]);
}

#[test]
fn field_then_index() {
    let program = compiled(".a[0]");
    assert_eq!(keys(&program), ["a"]);
    assert_eq!(indices(&program), [0]);
}

#[test]
fn bare_positive_index() {
    assert_eq!(indices(&compiled(".[2]")), [2]);
}

#[test]
fn negative_index_preserves_sign() {
    assert_eq!(indices(&compiled(".[-1]")), [-1]);
}

#[test]
fn deep_mixed_chain() {
    let program = compiled(r#".a.b[0]["c"][-2]"#);
    assert_eq!(keys(&program), ["a", "b", "c"]);
    assert_eq!(indices(&program), [0, -2]);
}

#[test]
fn forward_requirement_preserves_members_and_signed_indices() {
    let resources = resources();
    let program = compile(".items[-1]").expect("path");
    let requirement = program.try_requirement(&resources).expect("requirement");
    let steps = requirement.footprint().exact_path().expect("exact").steps();
    assert!(matches!(&steps[0], PortableStep::SemanticMember(m) if m == "items"));
    assert!(matches!(steps[1], PortableStep::SemanticIndex(-1)));
}

#[test]
fn forward_requirement_reanchors_kept_subtree_onto_the_located_node() {
    // Static prefix `.catalog`, residual `{id,name}`: the located
    // requirement carries a prune naming those two keys.
    let program = compiled(".catalog | {id,name}");
    assert_eq!(
        program.reanchor_prune_keys().as_deref(),
        Some([String::from("id"), String::from("name")].as_slice())
    );
    let resources = resources();
    let requirement = program.try_requirement(&resources).expect("requirement");
    assert_eq!(requirement.result(), AccessResultKind::Located);
    let steps = requirement.footprint().exact_path().expect("exact").steps();
    assert_eq!(steps.len(), 1);
    assert!(matches!(&steps[0], PortableStep::SemanticMember(m) if m == "catalog"));
    let tree = requirement.prune().expect("prune attached");
    let names: Vec<&str> = tree.root().members().map(|(name, _)| name).collect();
    assert_eq!(names, ["id", "name"]);
    assert!(
        tree.root().element().is_none(),
        "the located catalog object is not an iterated element"
    );

    // `.catalog[] | .name`: the residual starts at `.[]`. Re-anchoring
    // declines — the hint would not be rooted at the pushed-down node.
    let iterated = compiled(".catalog[] | .name");
    assert!(
        iterated.reanchor_prune_keys().is_none(),
        "re-anchor must decline when the residual starts at `.[]`"
    );

    // Static index in the prefix: follow the shared element node so residual
    // `{id,score}` omits unread members of the located object.
    let indexed = compiled(".users[0] | {id,score}");
    assert_eq!(
        indexed.reanchor_prune_keys().as_deref(),
        Some([String::from("id"), String::from("score")].as_slice())
    );
    let indexed_req = indexed.try_requirement(&resources).expect("requirement");
    assert_eq!(indexed_req.result(), AccessResultKind::Located);
    let indexed_steps = indexed_req.footprint().exact_path().expect("exact").steps();
    assert_eq!(indexed_steps.len(), 2);
    assert!(matches!(&indexed_steps[0], PortableStep::SemanticMember(m) if m == "users"));
    assert!(matches!(indexed_steps[1], PortableStep::SemanticIndex(0)));
    let indexed_names: Vec<&str> = indexed_req
        .prune()
        .expect("indexed construct prune")
        .root()
        .members()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(indexed_names, ["id", "score"]);
}

#[test]
fn identity_lowers_through_root_requirement() {
    // Identity must reach whole-document access, never a forward requirement
    // with an empty path (which would be an exact-root located selection).
    let requirement = requirement_of(".");
    assert_eq!(requirement.result(), AccessResultKind::CompleteDocument);
    assert!(requirement.footprint().is_whole());
}

#[test]
fn path_length_exact_locates_the_container() {
    // Nonempty PATH | length Exact-locates PATH so the count oracle reads the
    // located node. Bare `length` stays Whole (analysis/count.rs).
    for (source, key) in [(".users | length", "users"), (".catalog | length", "catalog")] {
        let requirement = requirement_of(source);
        assert!(requirement.count().is_some(), "{source} must carry the count hint");
        assert!(
            !requirement.footprint().is_whole(),
            "{source} Exact-locates the container"
        );
        assert_eq!(requirement.result(), AccessResultKind::Located, "{source}");
        let steps = requirement.footprint().exact_path().expect("exact").steps();
        assert_eq!(steps.len(), 1, "{source}");
        assert!(
            matches!(&steps[0], PortableStep::SemanticMember(m) if m == key),
            "{source} exact step"
        );
    }
}

#[test]
fn path_length_rebind_whole_keeps_count_and_prune() {
    let resources = resources();
    let program = compiled(".users | length");
    let exact = program.try_requirement(&resources).expect("exact");
    assert!(!exact.footprint().is_whole());
    assert!(exact.count().is_some());
    let rebind = program.try_rebind_whole_requirement(&resources).expect("rebind");
    assert!(rebind.footprint().is_whole(), "miss charge is Whole");
    assert!(rebind.count().is_some(), "ReboundWhole keeps the count hint");
    assert!(rebind.prune().is_some(), "ReboundWhole keeps the document-rooted prune");
}

#[test]
fn filter_collect_count_stays_exact_and_rebind_keeps_hints() {
    let source = ".users | map(select(.score > 100)) | length";
    let resources = resources();
    let program = compiled(source);
    let exact = program.try_requirement(&resources).expect("exact");
    assert!(exact.count().is_some(), "{source} is a collect-filter count");
    assert!(!exact.footprint().is_whole(), "{source} Exact-locates the container");
    let rebind = program.try_rebind_whole_requirement(&resources).expect("rebind");
    assert!(rebind.footprint().is_whole());
    assert!(rebind.count().is_some(), "filter miss keeps the count hint");
    assert!(rebind.prune().is_some(), "filter miss keeps prune");
}

#[test]
fn path_has_exact_locates_the_subject() {
    // PATH | has(LITERAL) Exact-locates PATH so the oracle reads the located
    // node. Bare `has("k")` stays Whole.
    let requirement = requirement_of(r#".users | has("id")"#);
    assert!(
        matches!(compiled(r#".users | has("id")"#).shortcut(), Shortcut::Has(_)),
        "PATH | has is a has row"
    );
    assert!(
        !requirement.footprint().is_whole(),
        "PATH | has Exact-locates the subject"
    );
    assert_eq!(requirement.result(), AccessResultKind::Located);
    let steps = requirement.footprint().exact_path().expect("exact").steps();
    assert_eq!(steps.len(), 1);
    assert!(matches!(&steps[0], PortableStep::SemanticMember(m) if m == "users"));

    let bare = requirement_of(r#"has("k")"#);
    assert!(matches!(compiled(r#"has("k")"#).shortcut(), Shortcut::Has(_)));
    assert!(bare.footprint().is_whole(), "bare has stays Whole");
}

#[test]
fn path_any_all_exact_locates_the_container() {
    // PATH | all(P) Exact-locates PATH so the oracle iterates the located
    // node. Bare `all(.ok)` stays Whole.
    let requirement = requirement_of(".users | all(.id)");
    assert!(
        matches!(compiled(".users | all(.id)").shortcut(), Shortcut::AnyAll(_)),
        "PATH | all is an any/all row"
    );
    assert!(
        !requirement.footprint().is_whole(),
        "PATH | all Exact-locates the container"
    );
    assert_eq!(requirement.result(), AccessResultKind::Located);
    let steps = requirement.footprint().exact_path().expect("exact").steps();
    assert_eq!(steps.len(), 1);
    assert!(matches!(&steps[0], PortableStep::SemanticMember(m) if m == "users"));

    let bare = requirement_of("all(.ok)");
    assert!(matches!(compiled("all(.ok)").shortcut(), Shortcut::AnyAll(_)));
    assert!(bare.footprint().is_whole(), "bare all stays Whole");

    let generated = requirement_of("all(.users[]; .id)");
    assert!(matches!(compiled("all(.users[]; .id)").shortcut(), Shortcut::AnyAll(_)));
    assert!(
        generated.footprint().is_whole(),
        "generator path is not a split prefix; empty prefix stays Whole"
    );

    assert!(
        !matches!(compiled(".a | all(.b[]; .k)").shortcut(), Shortcut::AnyAll(_)),
        "pipe prefix plus generator path is not a row"
    );
}

#[test]
fn path_min_max_exact_locates_the_container() {
    // PATH | min Exact-locates PATH so the oracle reads the located node.
    // Bare `min` stays Whole.
    let requirement = requirement_of(".xs | min");
    assert!(
        matches!(compiled(".xs | min").shortcut(), Shortcut::MinMax(_)),
        "PATH | min is a min/max row"
    );
    assert!(
        !requirement.footprint().is_whole(),
        "PATH | min Exact-locates the container"
    );
    assert!(requirement.minmax().is_some(), "PATH | min packs the minmax hint");
    assert_eq!(requirement.result(), AccessResultKind::Located);
    let steps = requirement.footprint().exact_path().expect("exact").steps();
    assert_eq!(steps.len(), 1);
    assert!(matches!(&steps[0], PortableStep::SemanticMember(m) if m == "xs"));

    let bare = requirement_of("min");
    assert!(matches!(compiled("min").shortcut(), Shortcut::MinMax(_)));
    assert!(bare.footprint().is_whole(), "bare min stays Whole");

    let by = requirement_of(".xs | min_by(.n)");
    assert!(matches!(compiled(".xs | min_by(.n)").shortcut(), Shortcut::MinMax(_)));
    assert!(!by.footprint().is_whole(), "PATH | min_by Exact-locates");
}

#[test]
fn finish_commits_one_shortcut() {
    use super::Access;
    assert!(matches!(compiled(".").shortcut(), Shortcut::Identity));
    assert!(matches!(compiled("length").shortcut(), Shortcut::Count(_)));
    assert_eq!(compiled("length").access(), Access::Whole);
    assert!(matches!(
        compiled(".users[] | .id").shortcut(),
        Shortcut::Element { .. }
    ));
    assert_eq!(compiled(".users[] | .id").access(), Access::Exact);
    assert_eq!(compiled(".[] | .id").access(), Access::Whole);
    assert_eq!(compiled(".users | length").access(), Access::Exact);
    assert!(matches!(
        compiled("[.users[].name] | length").shortcut(),
        Shortcut::Count(_)
    ));
    assert_eq!(compiled("[.users[].name] | length").access(), Access::Exact);
    assert!(matches!(compiled("keys").shortcut(), Shortcut::Keys(_)));
    assert!(matches!(compiled("type").shortcut(), Shortcut::Type(_)));
    assert!(matches!(compiled(r#"has("k")"#).shortcut(), Shortcut::Has(_)));
    assert!(matches!(compiled("all(.ok)").shortcut(), Shortcut::AnyAll(_)));
    assert!(matches!(compiled("min").shortcut(), Shortcut::MinMax(_)));
    assert!(matches!(compiled(".[1:3]").shortcut(), Shortcut::RangeLocate));
    assert_eq!(compiled(".[1:3]").access(), Access::Whole);
    assert_eq!(compiled(".catalog[1:3]").access(), Access::Exact);
    assert!(matches!(compiled(".a + .b").shortcut(), Shortcut::None));
}

#[test]
fn finish_pins_the_shortcut_across_consultations() {
    let compiled = compiled("[.catalog[] | .id] | length");
    let first = compiled.shortcut();
    for _ in 0..64 {
        let _ = compiled.shortcut();
        let _ = compiled.host_io();
    }
    assert!(core::ptr::eq(first, compiled.shortcut()));
    assert!(matches!(compiled.shortcut(), Shortcut::Count(_)));
}

#[test]
fn packed_requirement_matches_the_old_ladder() {
    use crate::HostIo;

    let identity = requirement_of(".");
    assert!(identity.footprint().is_whole(), "identity is Whole");
    assert!(identity.count().is_none());
    assert!(identity.element().is_none());
    assert!(!identity.type_demand());
    assert_eq!(compiled(".").host_io(), HostIo::Echo);

    let whole_count = requirement_of("length");
    assert!(whole_count.footprint().is_whole(), "bare length is Whole");
    assert!(whole_count.count().is_some(), "bare length carries the count hint");
    assert!(whole_count.element().is_none());
    assert!(!whole_count.type_demand());
    assert_eq!(whole_count.result(), AccessResultKind::CompleteDocument);

    let exact_count = requirement_of(".users | length");
    assert!(!exact_count.footprint().is_whole(), "PATH | length is Exact");
    assert!(exact_count.count().is_some(), "PATH | length carries the count hint");
    assert_eq!(exact_count.result(), AccessResultKind::Located);
    let steps = exact_count.footprint().exact_path().expect("exact").steps();
    assert_eq!(steps.len(), 1);
    assert!(matches!(&steps[0], PortableStep::SemanticMember(m) if m == "users"));

    let exact_range = requirement_of(".catalog[1:3]");
    assert!(
        !exact_range.footprint().is_whole(),
        "PATH[a:b] Exact-locates the prefix"
    );
    assert!(exact_range.count().is_none());
    assert!(exact_range.element().is_none());
    assert!(!exact_range.type_demand());
    assert_eq!(exact_range.result(), AccessResultKind::Located);
    let steps = exact_range.footprint().exact_path().expect("exact").steps();
    assert_eq!(steps.len(), 1);
    assert!(matches!(&steps[0], PortableStep::SemanticMember(m) if m == "catalog"));
    assert_eq!(compiled(".catalog[1:3]").host_io(), HostIo::SpanCut);
    let resources = resources();
    let range = compiled(".catalog[1:3]")
        .try_range_locate_requirement(&resources)
        .expect("range-locate charges the packed slice path");
    let range_steps = range.footprint().exact_path().expect("exact").steps();
    assert!(
        range_steps
            .iter()
            .any(|step| matches!(step, PortableStep::SemanticRange { .. })),
        "range-locate requirement keeps the trailing range"
    );

    let residual = requirement_of(".a + .b");
    assert!(residual.count().is_none(), "residual has no count hint");
    assert!(residual.element().is_none(), "residual has no element hint");
    assert!(!residual.type_demand(), "residual has no type hint");
    assert_eq!(compiled(".a + .b").host_io(), HostIo::Run);

    let exact_element = requirement_of(".users[] | .id");
    assert!(
        exact_element.element().is_some(),
        "PATH[] | .id Exact still carries the element hint"
    );
    assert!(!exact_element.footprint().is_whole(), "nonempty element path is Exact");
    assert_eq!(
        exact_element.lazy_frontier(),
        1,
        "fan-out Exact stays lazy so the span leaf can extract"
    );
    assert_eq!(compiled(".users[] | .id").host_io(), HostIo::Run);

    let construct = requirement_of(".users | map({id,score})");
    assert!(construct.element().is_some(), "map({{id,score}}) is an element row");
    assert!(
        construct.element_construct().is_some(),
        "map({{id,score}}) packs construct fields for the locate walk"
    );
    assert!(!construct.footprint().is_whole(), "PATH | map construct Exact-locates");

    let root_fields = requirement_of("{id,score}");
    assert!(root_fields.footprint().is_whole(), "root construct is Whole");
    let root_names: Vec<&str> = root_fields
        .prune()
        .expect("root construct prune")
        .root()
        .members()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(root_names, ["id", "score"]);
}

#[test]
fn a_piped_identity_does_not_drag_the_whole_document_in() {
    // `.` is `|`'s unit, and the lowering folds it out — which matters
    // because a bare identity stage IS a whole-document requirement (the
    // test above). Left on the spine it defeated every downstream
    // projection: `. | .users | length` decoded a 9 MB fixture in 77ms
    // where `.users | length` decoded one member in 36ms.
    for source in [". | .a.b", ".a.b | .", "(.) | .a.b", ". | . | .a.b"] {
        let requirement = requirement_of(source);
        assert_eq!(
            requirement.result(),
            AccessResultKind::Located,
            "{source} must project like the bare path"
        );
        assert!(!requirement.footprint().is_whole(), "{source}");
    }
}

#[test]
fn the_corpus_floor_forcing_wrapper_takes_the_whole_document_route() {
    // The floor is forced by wrapping the authored filter as
    // `[.][0] | (P)` — the spelling the `floorparity` rows and the sdk-smoke
    // projection-vs-floor oracle both use. Those comparisons are only worth
    // something if the wrapper genuinely defeats the pushdown, which is
    // exactly what this pins: a constructor on the upstream spine pushes
    // NOTHING down, so the wrapped requirement is whole-document while the
    // bare filter's is a located exact path.
    for (bare_source, floor_source) in [
        (".a.b", "[.][0] | (.a.b)"),
        (".catalog[0].id", "[.][0] | (.catalog[0].id)"),
    ] {
        let bare = requirement_of(bare_source);
        assert_eq!(
            bare.result(),
            AccessResultKind::Located,
            "{bare_source} must take a located route unwrapped"
        );
        assert!(!bare.footprint().is_whole());

        let floor = requirement_of(floor_source);
        assert_eq!(
            floor.result(),
            AccessResultKind::CompleteDocument,
            "{floor_source} must take the whole-document floor"
        );
        assert!(floor.footprint().is_whole());
    }
    // `.catalog[].name` is an ELEMENT row with a nonempty PATH: finish
    // Exact-locates `.catalog` and still carries the element hint so the
    // span skeleton survives for the document-core consumer. The wrapper
    // keeps its own whole-document floor (constructor on the spine).
    let bare = requirement_of(".catalog[].name");
    assert_eq!(
        bare.result(),
        AccessResultKind::Located,
        "the fan-out row Exact-locates the container"
    );
    assert!(!bare.footprint().is_whole());
    assert!(
        bare.element().is_some(),
        "the fan-out row must carry the element demand hint"
    );
    let floor = requirement_of("[.][0] | (.catalog[].name)");
    assert_eq!(
        floor.result(),
        AccessResultKind::CompleteDocument,
        "the wrapper must take the whole-document floor"
    );
    assert!(floor.footprint().is_whole());
}

#[test]
fn nested_keys_lower_to_exact_members() {
    let requirement = requirement_of(".a.b");
    assert_eq!(requirement.result(), AccessResultKind::Located);
    let steps = requirement.footprint().exact_path().expect("exact").steps();
    assert_eq!(steps.len(), 2);
    assert!(matches!(&steps[0], PortableStep::SemanticMember(m) if m == "a"));
    assert!(matches!(&steps[1], PortableStep::SemanticMember(m) if m == "b"));
}

#[test]
fn quoted_and_unicode_keys_lower_decoded() {
    let quoted = requirement_of(r#"."quoted key""#);
    let steps = quoted.footprint().exact_path().expect("exact").steps();
    assert_eq!(steps.len(), 1);
    assert!(matches!(&steps[0], PortableStep::SemanticMember(m) if m == "quoted key"));

    let unicode = requirement_of(r#"."café""#);
    let steps = unicode.footprint().exact_path().expect("exact").steps();
    assert_eq!(steps.len(), 1);
    assert!(matches!(&steps[0], PortableStep::SemanticMember(m) if m == "caf\u{e9}"));
}

#[test]
fn bracket_string_key_lowers_to_member() {
    let requirement = requirement_of(r#".["a"]"#);
    let steps = requirement.footprint().exact_path().expect("exact").steps();
    assert_eq!(steps.len(), 1);
    assert!(matches!(&steps[0], PortableStep::SemanticMember(m) if m == "a"));
}

#[test]
fn positive_and_negative_indices_preserve_sign() {
    let positive = requirement_of(".[2]");
    let steps = positive.footprint().exact_path().expect("exact").steps();
    assert!(matches!(steps[0], PortableStep::SemanticIndex(2)));

    let negative = requirement_of(".[-1]");
    let steps = negative.footprint().exact_path().expect("exact").steps();
    assert!(matches!(steps[0], PortableStep::SemanticIndex(-1)));
}

#[test]
fn deep_mixed_chain_lowers_in_order() {
    let requirement = requirement_of(r#".a.b[0]["c"][-2]"#);
    assert_eq!(requirement.result(), AccessResultKind::Located);
    let steps = requirement.footprint().exact_path().expect("exact").steps();
    assert_eq!(steps.len(), 5);
    assert!(matches!(&steps[0], PortableStep::SemanticMember(m) if m == "a"));
    assert!(matches!(&steps[1], PortableStep::SemanticMember(m) if m == "b"));
    assert!(matches!(steps[2], PortableStep::SemanticIndex(0)));
    assert!(matches!(&steps[3], PortableStep::SemanticMember(m) if m == "c"));
    assert!(matches!(steps[4], PortableStep::SemanticIndex(-2)));
}

#[test]
fn dynamic_string_selector_folds_to_a_static_accessor_step() {
    // A hole-free string in `.@(…)` / `.&(…)` is the same compile-time
    // name the direct form carries — the `.[expr]` constant-fold arm.
    let node = compiled(r#".@("tag")"#);
    assert!(matches!(
        root_stage_steps(&node)[0].access(),
        StepAccess::NodeAccessor(name) if name == "tag"
    ));
    let alias = compiled(r#".@("comment_head")"#);
    assert!(matches!(
        root_stage_steps(&alias)[0].access(),
        StepAccess::NodeAccessor(name) if name == "comment"
    ));
    let attribute = compiled(r#".&("href")"#);
    assert!(matches!(
        root_stage_steps(&attribute)[0].access(),
        StepAccess::Attribute(name) if name == "href"
    ));
}

#[test]
fn dynamic_variable_selector_lowers_to_a_dyn_accessor_step() {
    // A bare `$name` is the slot itself, not a `DynVar` key/index step.
    let node = compiled(r#""tag" as $name | .@($name)"#);
    let nodes = node.program.nodes();
    let ProgramNode::Bind { slot, body, .. } = &nodes[node.program.root().index()] else {
        panic!("a bound selector must lower to a binder frame");
    };
    let ProgramNode::Stage { steps, .. } = &nodes[body.index()] else {
        panic!("the frame must wrap the accessor stage");
    };
    assert!(matches!(
        steps[0].access(),
        StepAccess::DynNodeAccessor(bound) if bound == slot
    ));

    let attribute = compiled(r#""href" as $attr | .&($attr)"#);
    let nodes = attribute.program.nodes();
    let ProgramNode::Bind { slot, body, .. } = &nodes[attribute.program.root().index()] else {
        panic!("a bound attribute selector must lower to a binder frame");
    };
    let ProgramNode::Stage { steps, .. } = &nodes[body.index()] else {
        panic!("the frame must wrap the attribute stage");
    };
    assert!(matches!(
        steps[0].access(),
        StepAccess::DynAttribute(bound) if bound == slot
    ));
}

#[test]
fn compiled_dynamic_tag_read_matches_static() {
    use crate::exec::{EngineRun, RunPoll};
    use jqf_builtins::codec_result::{CodecInputOutcome, EngineResult};
    let mut resources = resources();
    let tagged = Value::try_tagged(
        jqf_data::TagId::try_new_unaccounted("!money").expect("tag"),
        Value::Null,
    )
    .expect("tagged");
    let mut collect = |program: &str| {
        let compiled = compiled(program);
        let mut stream = match compiled
            .try_run_whole_value(
                CodecInputOutcome::Result(EngineResult::owned(tagged.clone())),
                &resources,
            )
            .expect("run")
        {
            EngineRun::Stream { stream, .. } => stream,
            other => panic!("{program}: {other:?}"),
        };
        match stream.poll(&mut resources) {
            Ok(RunPoll::Item(EngineResult::Owned(Value::String(text)))) => String::from(text.as_str()),
            other => panic!("{program}: {other:?}"),
        }
    };
    assert_eq!(collect(".@tag"), "!money");
    assert_eq!(collect(r#".@("tag")"#), "!money");
    assert_eq!(collect(r#""tag" as $name | .@($name)"#), "!money");
}

#[test]
fn user_empty_shadows_the_syntax_form_and_literals_do_not() {
    // A user `empty/0` shadows the syntax form; bare `true`/`false`/`null`
    // stay literals even when a matching def exists. `true(5)` is a call.
    use crate::exec::{EngineRun, RunPoll};
    use jqf_builtins::codec_result::{CodecInputOutcome, EngineResult};
    let mut resources = resources();
    let mut collect = |program: &str| -> Vec<String> {
        let compiled = compiled(program);
        let mut stream = match compiled
            .try_run_whole_value(CodecInputOutcome::Result(EngineResult::owned(Value::Null)), &resources)
            .expect("run")
        {
            EngineRun::Stream { stream, .. } => stream,
            other => panic!("{program}: {other:?}"),
        };
        let mut out = Vec::new();
        loop {
            match stream.poll(&mut resources) {
                Ok(RunPoll::Item(EngineResult::Owned(value))) => {
                    out.push(jqf_builtins::semantics::render::to_json(&value).expect("json"));
                }
                Ok(RunPoll::Complete) => break,
                other => panic!("{program}: {other:?}"),
            }
        }
        out
    };
    assert_eq!(collect("def empty: 1; empty"), ["1"]);
    assert_eq!(collect("def true: 1; true"), ["true"]);
    assert_eq!(collect("def true($x): $x; true(5)"), ["5"]);
    assert_eq!(
        collect("def empty($x): $x; empty"),
        Vec::<String>::new(),
        "bare empty with only empty/1 in scope is still the syntax form"
    );
}

#[test]
fn dynamic_selector_writes_compile_to_fact_assigns() {
    // A dynamic selector on a WRITE lowers like its static
    // twin — a hole-free string folds STATIC (`.@("comment")` ≡ `.@comment`),
    // and a real expression binds outside the write for runtime validation
    // against the closed vocabulary. Lane 2 rides beside it: computed-base
    // targets (`1 as $i | .[$i].@comment`) lower through the same node.
    for program in [
        r#"."c" as $r | .@($r) = "x""#,
        r#".@("tag") = "x""#,
        r#".@("comment") = ["x"]"#,
        r#"."h" as $h | .&($h) |= "u""#,
        r#".&("href") |= "u""#,
        r#"."k" as $k | .[$k].@comment = ["x"]"#,
        r#"1 as $i | .[$i].&href = "u""#,
    ] {
        let compiled = compiled(program);
        assert!(compiled.fact_writes(), "{program} must lower to a FactAssign");
    }
}

#[test]
fn accessor_writes_that_are_not_the_last_step_stay_rejected() {
    // Nested/dotted fact paths are rejected: the role
    // vocabulary is flat, so `.name.@comment.leading` names no role. An
    // accessor left INSIDE the prefix fails the key/index-chain check.
    assert_eq!(
        unsupported(r#".name.@comment.leading = "x""#),
        UnsupportedConstruct::AccessorAssignment
    );
    match unsupported(r#".a.@tag.@comment = "x""#) {
        UnsupportedConstruct::Expression(message) => {
            assert!(message.contains("non-static path"), "{message}");
        }
        other => panic!("expected the path-shape rejection, got {other:?}"),
    }
}

#[test]
fn rejects_assignment_over_accessor_paths() {
    // A non-admitted selector like `.@bogus` stays the deferred-write
    // rejection. Admitted fact targets (`.@comment`, `.&href`) compile.
    assert_eq!(
        unsupported(r#".a.@bogus = "x""#),
        UnsupportedConstruct::AccessorAssignment
    );
    let href = compiled(r#".a.&href |= "u""#);
    assert!(href.fact_writes(), "an admitted attribute write lowers to FactAssign");
}

#[test]
fn admitted_fact_roles_compile_without_edit_mode() {
    // The write allow-list admits the three comment-position selectors
    // plus the four METADATA roles of the YAML write channel. Query-time
    // compilation lowers each to FactAssign; `--edit` is the splice lane,
    // not a compile gate.
    for program in [
        r#".a.@comment = ["x"]"#,
        r#".a.@comment_inline = ["x"]"#,
        r#".a.@comment_foot = ["x"]"#,
        r#".a.@style = "double""#,
        r#".a.@tag = "!!str""#,
        r#".a.@anchor = "x""#,
        r#".a.@alias = "x""#,
    ] {
        let compiled = compiled(program);
        assert!(compiled.fact_writes(), "{program} must lower to a FactAssign");
    }
}

#[test]
fn specific_spelling_rejections_skip_the_supported_surface_dump() {
    // rejection whose `describe()` names ONE exact spelling (the accessor
    // family) leads with its own sentence and skips the supported-surface
    // dump. A one-token mistake like `.a.@bogus = "x"` must not print an
    // ~8 KB wall of every registered builtin. (Dynamic selectors COMPILE;
    // only the non-admitted static names stay here.)
    let rendered = format!("{}", unsupported_error(r#".a.@bogus = "x""#));
    assert!(
        !rendered.contains("supported surface"),
        "the rejection must not carry the supported-construct dump: {rendered}"
    );
    assert!(
        !rendered.contains("builtins:"),
        "the rejection must not name the builtin list: {rendered}"
    );
}

#[test]
fn node_accessor_lowers_to_an_accessor_step() {
    // The direct `.@name` and quoted `.@["name"]` forms now lower to real
    // `StepAccess::NodeAccessor` steps carrying the decoded selector text.
    let direct = compiled(".@tag");
    assert!(matches!(root_stage_steps(&direct), [StageStep { .. }]));
    assert!(matches!(
        root_stage_steps(&direct)[0].access(),
        StepAccess::NodeAccessor(name) if name == "tag"
    ));

    let quoted = compiled(r#".@["comment"]"#);
    assert!(matches!(
        root_stage_steps(&quoted)[0].access(),
        StepAccess::NodeAccessor(name) if name == "comment"
    ));

    // An escaped quoted selector decodes with the escape set.
    let escaped = compiled(r#".@["leading\nnote"]"#);
    assert!(matches!(
        root_stage_steps(&escaped)[0].access(),
        StepAccess::NodeAccessor(name) if name == "leading\nnote"
    ));
}

#[test]
fn attribute_accessor_lowers_to_an_attribute_step() {
    let direct = compiled(".&href");
    assert!(matches!(
        root_stage_steps(&direct)[0].access(),
        StepAccess::Attribute(name) if name == "href"
    ));

    let quoted = compiled(r#".&["data-id"]"#);
    assert!(matches!(
        root_stage_steps(&quoted)[0].access(),
        StepAccess::Attribute(name) if name == "data-id"
    ));
}

#[test]
fn accessor_steps_compose_with_ordinary_postfix_paths() {
    // `.price.@tag` and `.a.&href` are one fused stage: an ordinary key step
    // followed by the accessor step.
    let node = compiled(".price.@tag");
    assert!(matches!(root_stage_steps(&node), [StageStep { .. }, StageStep { .. }]));
    assert!(matches!(
        root_stage_steps(&node)[0].access(),
        StepAccess::Key(name) if name == "price"
    ));
    assert!(matches!(
        root_stage_steps(&node)[1].access(),
        StepAccess::NodeAccessor(name) if name == "tag"
    ));

    let attribute = compiled(".a.&href");
    assert!(matches!(
        root_stage_steps(&attribute)[0].access(),
        StepAccess::Key(name) if name == "a"
    ));
    assert!(matches!(
        root_stage_steps(&attribute)[1].access(),
        StepAccess::Attribute(name) if name == "href"
    ));
}

#[test]
fn iteration_compiles_to_an_each_step() {
    // `.[]` is now in the subset: a single `Each` step, whole-document
    // pushdown (empty prefix), the entire stage as residual.
    let program = compiled(".[]");
    assert!(program.program.split().is_whole_document());
    assert_eq!(program.program.split().prefix_len(), 0);
    assert_eq!(root_stage_steps(&program).len(), 1);
}

#[test]
fn prefixed_iteration_pushes_the_prefix_and_keeps_the_residual() {
    // `.a[].b`: prefix `.a` pushed down (len 1); residual `[Each, b]`.
    let program = compiled(".a[].b");
    assert!(!program.program.split().is_whole_document());
    assert_eq!(program.program.split().prefix_len(), 1);
    assert_eq!(root_stage_steps(&program).len(), 3);
}

#[test]
fn prefixed_iteration_exact_locates_the_container() {
    use super::Access;
    // `.a[].b` is an ELEMENT row with a nonempty PATH. Finish commits Exact
    // of `.a` so the oracle starts at `located.node()`. Empty `.[]` stays
    // Whole — it cannot Exact-walk. Packed Exact carries the element hint.
    let prefixed = requirement_of(".a[].b");
    let bare = requirement_of(".a");
    assert_eq!(compiled(".a[].b").access(), Access::Exact);
    assert!(!prefixed.footprint().is_whole(), ".a[].b Exact-locates `.a`");
    assert!(prefixed.element().is_some(), "Exact element still carries the hint");
    assert_eq!(prefixed.result(), AccessResultKind::Located);
    let steps = prefixed.footprint().exact_path().expect("exact").steps();
    assert_eq!(steps.len(), 1);
    assert!(matches!(&steps[0], PortableStep::SemanticMember(m) if m == "a"));
    assert!(!bare.footprint().is_whole());
    assert_eq!(bare.result(), AccessResultKind::Located);

    let root = requirement_of(".[] | .id");
    assert_eq!(compiled(".[] | .id").access(), Access::Whole);
    assert!(root.footprint().is_whole(), "`.[]` at root stays Whole");
    assert!(root.element().is_some());
}

#[test]
fn a_general_slice_bound_becomes_a_binder_frame() {
    // Both bounds take the index operand's frame, and `start` ends up OUTSIDE
    // `end`: `start` is generated first, so `end`'s forks are younger and it
    // varies faster (`[0,1,2,3,4] | [.[(0,1):(3,4)]]` is
    // `[[0,1,2],[0,1,2,3],[1,2],[1,2,3]]`).
    let program = compiled(".[.a:.b]");
    let nodes = program.program.nodes();
    let ProgramNode::Bind {
        slot: start_slot, body, ..
    } = &nodes[program.program.root().index()]
    else {
        panic!("a general slice bound must lower to a binder frame");
    };
    let ProgramNode::Bind {
        slot: end_slot,
        body: stage,
        ..
    } = &nodes[body.index()]
    else {
        panic!("`start` must sit outside `end`");
    };
    let ProgramNode::Stage { steps, .. } = &nodes[stage.index()] else {
        panic!("the frames must wrap the slice's stage");
    };
    let StepAccess::Slice(bounds) = steps[0].access() else {
        panic!("the step must stay a slice");
    };
    assert!(matches!(bounds.start, SliceBound::Var(slot) if slot == *start_slot));
    assert!(matches!(bounds.end, SliceBound::Var(slot) if slot == *end_slot));
}

#[test]
fn a_constant_slice_bound_still_needs_no_binder() {
    // `.[1:2]`, an open bound and a bare `$var` keep their verbatim spellings,
    // which is what every landed slice-route receipt keys off.
    for source in [".[1:2]", ".[:2]", ".[-1:]", "1 as $x | .[$x:]"] {
        let program = compiled(source);
        let binders = program
            .program
            .nodes()
            .iter()
            .filter(|node| matches!(node, ProgramNode::Bind { .. }))
            .count();
        assert_eq!(
            binders,
            usize::from(source.contains(" as ")),
            "{source} must not grow a binder"
        );
    }
}

/// The authored bound pair of the program's ONE slice step (panics
/// otherwise), as the bounds the pushdown reads them — the very object
/// `try_normalize` / the recognizers consume.
fn one_slice(program: &CompiledProgram) -> &crate::program::SliceBounds {
    let mut found: Option<&crate::program::SliceBounds> = None;
    for node in program.program.nodes() {
        let ProgramNode::Stage { steps, .. } = node else {
            continue;
        };
        for step in steps {
            if let StepAccess::Slice(bounds) = step.access() {
                assert!(found.is_none(), "expected exactly one slice step");
                found = Some(bounds);
            }
        }
    }
    found.expect("expected exactly one slice step")
}

#[test]
fn a_computed_constant_slice_bound_folds_to_its_literal() {
    // `.[(1+2):]` folds to the authored `.[3:]` at lower time,
    // so the range-projection early stop and every recognizer that keys off
    // an authored literal bound engage without change. The fold is exact —
    // the SAME arithmetic the executor runs, so the literal IS what the
    // runtime frame would have produced.
    let program = compiled(".[(1+2):]");
    let bounds = one_slice(&program);
    assert_eq!(bounds.try_normalize(), Some((Some(3), None)));
    let program = compiled(".[(10-2):(1+2)]");
    let bounds = one_slice(&program);
    assert_eq!(bounds.try_normalize(), Some((Some(8), Some(3))));
    // Nested groups fold through the same set: `((1+2)+3)` is 6.
    let program = compiled(".[((1+2)+3):(2+2)]");
    let bounds = one_slice(&program);
    assert_eq!(bounds.try_normalize(), Some((Some(6), Some(4))));
    // `(0-1)` folds to the authored -1 — a folded negative bound normalizes
    // exactly as an authored `.[-1:]` does: the signed value rides the
    // range and the codec's two-pass arm resolves it against the observed
    // container length.
    let program = compiled(".[(0-1):]");
    let bounds = one_slice(&program);
    assert!(matches!(bounds.start, SliceBound::Literal(_)));
    assert_eq!(bounds.try_normalize(), Some((Some(-1), None)));
}

#[test]
fn a_constant_bound_variable_folds_to_its_binding() {
    // `1 as $x | .[$x:]` folds the slice bound to the literal the binder
    // provably delivers — byte-for-byte the authored `.[1:]`. The binder's
    // own `Bind` frame still runs; only the slice's slot read is replaced.
    let program = compiled("1 as $x | .[$x:]");
    let bounds = one_slice(&program);
    assert_eq!(bounds.try_normalize(), Some((Some(1), None)));
    // Both bounds, and the innermost binder wins on a shadowing re-bind.
    let program = compiled("2 as $x | 1 as $y | .[$y:$x]");
    let bounds = one_slice(&program);
    assert_eq!(bounds.try_normalize(), Some((Some(1), Some(2))));
    let program = compiled("1 as $x | 2 as $x | .[$x:]");
    let bounds = one_slice(&program);
    assert_eq!(bounds.try_normalize(), Some((Some(2), None)));
    // A non-number constant binding folds too: `null` behaves as Open at
    // runtime, exactly as the Var path would have resolved it.
    let program = compiled("null as $x | .[$x:]");
    let bounds = one_slice(&program);
    assert!(matches!(bounds.start, SliceBound::Literal(Value::Null)));
    assert_eq!(bounds.try_normalize(), Some((None, None)));
    // The fold does NOT require the bound to be usable in the pushdown: a
    // string binding folds to its authored literal and declines exactly as
    // an authored non-numeric bound does (the runtime raises the
    // integer-bound class over an array either way).
    let program = compiled("\"abc\" as $x | .[$x:2]");
    let bounds = one_slice(&program);
    assert!(matches!(bounds.start, SliceBound::Literal(Value::String(_))));
    assert_eq!(bounds.try_normalize(), None);
}

#[test]
fn a_dynamic_bound_variable_stays_a_var_slot() {
    // The decline discipline: anything the fold cannot prove stays dynamic,
    // exactly as before. A bind whose source is not a lower-time constant
    // (an input read, a multi-output generator, a destructuring projection,
    // a function argument) leaves the slice bound a `Var` slot.
    for source in [
        ".[0] as $x | .[$x:]",
        "(1,2) as $x | .[$x:]",
        "{a:1} as {$x} | .[$x:]",
        "def f($x): .[$x:]; f(3)",
    ] {
        let program = compiled(source);
        let bounds = one_slice(&program);
        assert!(
            matches!(bounds.start, SliceBound::Var(_)),
            "{source} must keep a Var bound"
        );
        assert_eq!(bounds.try_normalize(), None, "{source} must decline");
    }
    // A destructuring projection DOES bind a slot (the frame exists); only
    // the fold declines — the runtime value is still the projected member.
    let program = compiled("{a:1} as {$x} | .[$x:]");
    assert!(
        program
            .program
            .nodes()
            .iter()
            .any(|node| matches!(node, ProgramNode::Bind { .. }))
    );
}

#[test]
fn a_non_constant_computed_bound_still_takes_a_binder_frame() {
    // The `+`/`-` fold declines every operand that is not a provably-
    // constant NUMBER: an input read, a non-additive operator, a non-number
    // operand (string concat, the null additive identity — the latter fires
    // a mismatch cell at runtime, which a fold must never skip), and a bound
    // variable inside the expression.
    for source in [
        ".[(.a+1):]",
        ".[(1*2):]",
        ".[(1/2):]",
        ".[(1-\"a\"):]",
        ".[(\"a\"+\"b\"):]",
        ".[(null+1):]",
        "2 as $x | .[($x+1):]",
    ] {
        let program = compiled(source);
        let bounds = one_slice(&program);
        assert!(
            matches!(bounds.start, SliceBound::Var(_)),
            "{source} must keep a Var bound"
        );
        assert_eq!(bounds.try_normalize(), None, "{source} must decline");
        assert!(
            program
                .program
                .nodes()
                .iter()
                .any(|node| matches!(node, ProgramNode::Bind { .. })),
            "{source} must keep its operand frame"
        );
    }
}

#[test]
fn a_folded_computed_bound_matches_its_authored_literal_requirement() {
    // The range-projection law, at the route-eligibility level: a folded
    // computed bound is INDISTINGUISHABLE from the authored literal — same
    // eligibility, same lowered exact-path footprint, same result
    // authority. This is the byte-identity the corpus pins at run time.
    let resources = resources();
    for (folded, authored) in [(".[(1+2):]", ".[3:]"), (".catalog[(10-2):(1+2)]", ".catalog[8:3]")] {
        assert_eq!(compiled(folded).host_io(), HostIo::SpanCut, "{folded} must route");
        let folded_req = compiled(folded)
            .try_range_locate_requirement(&resources)
            .expect("the folded range-locate requirement lowers");
        let authored_req = compiled(authored)
            .try_range_locate_requirement(&resources)
            .expect("the authored range-locate requirement lowers");
        assert_eq!(
            folded_req.footprint().fingerprint(),
            authored_req.footprint().fingerprint(),
            "{folded} must lower the same footprint as {authored}"
        );
        assert_eq!(folded_req.result(), authored_req.result());
    }
    // The decline twin: a non-foldable computed bound keeps the floor — it
    // is not silently given the literal's fast lane.
    assert_ne!(compiled(".[(1*2):]").host_io(), HostIo::SpanCut);
}

#[test]
fn per_component_optional_suffix_is_accepted_and_flagged() {
    // `.a?` compiles: one key step carrying the per-component optional flag.
    assert_eq!(keys(&compiled(".a?")), ["a"]);
    assert_eq!(optional_flags(&compiled(".a?")), [true]);
}

#[test]
fn optional_flag_is_per_component_within_a_chain() {
    // `.a?.b` flags only the first component; `.a.b?` flags only the second.
    assert_eq!(keys(&compiled(".a?.b")), ["a", "b"]);
    assert_eq!(optional_flags(&compiled(".a?.b")), [true, false]);
    assert_eq!(optional_flags(&compiled(".a.b?")), [false, true]);
}

#[test]
fn optional_suffix_on_indices_and_bracket_keys() {
    assert_eq!(indices(&compiled(".[0]?")), [0]);
    assert_eq!(optional_flags(&compiled(".[0]?")), [true]);
    assert_eq!(keys(&compiled(r#".["a"]?"#)), ["a"]);
    assert_eq!(optional_flags(&compiled(r#".["a"]?"#)), [true]);
}

#[test]
fn standalone_term_suppression_lowers_to_a_catchless_try() {
    // FENCE FLIP: `.?` is the whole-term `try` sugar — now legal, a
    // catchless `Try` over identity (`5 | .?` → 5).
    let program = compiled(".?");
    assert!(matches!(
        program.program.nodes()[program.program.root().index()],
        ProgramNode::Try { handler: None, .. }
    ));
}

#[test]
fn second_optional_lowers_as_term_suppression_try() {
    // FENCE FLIP: the first `?` of `.a??` is the per-component flag; the second
    // is a whole-term `try` — now a catchless `Try` wrapping the `.a?` prefix.
    let program = compiled(".a??");
    assert!(matches!(
        program.program.nodes()[program.program.root().index()],
        ProgramNode::Try { handler: None, .. }
    ));
}

#[test]
fn a_general_index_operand_becomes_a_binder_frame() {
    // `.[e]` is `e as $t | .[$t]`: the operand is hoisted into a `Bind` whose
    // body indexes through the slot it just filled. An interpolated field key
    // (`."\(1)"`) is the same form under different punctuation, and a
    // non-integer numeric index is just an operand the runtime truncates.
    for source in [".[.a]", ".[1+1]", ".[1.5]", ".[-1.5]", r#"."\(1)""#, r#".["a"+"b"]"#] {
        let program = compiled(source);
        let nodes = program.program.nodes();
        let ProgramNode::Bind { slot, body, .. } = &nodes[program.program.root().index()] else {
            panic!("{source} must lower to a binder frame");
        };
        let ProgramNode::Stage { steps, .. } = &nodes[body.index()] else {
            panic!("{source} must bind a stage");
        };
        assert!(
            matches!(steps[0].access(), StepAccess::DynVar(bound) if bound == slot),
            "{source} must read the slot it bound"
        );
    }
}

#[test]
fn a_constant_index_operand_still_needs_no_binder() {
    // The fast paths that keep `.a[0]`, `.["k"]` and `.[$x]` at exactly their
    // pre-vertical cost: one fused step, and no binder frame beyond the ones
    // the user authored.
    for source in [".[0]", ".[-1]", r#".["k"]"#, ".a[0]", "1 as $x | .[$x]"] {
        let program = compiled(source);
        let binders = program
            .program
            .nodes()
            .iter()
            .filter(|node| matches!(node, ProgramNode::Bind { .. }))
            .count();
        let authored = usize::from(source.contains(" as "));
        assert_eq!(binders, authored, "{source} must not grow a binder");
    }
}

#[test]
fn recursive_descent_lowers_to_one_descend_step() {
    // `..` is in scope now: it lowers to a current-start stage carrying one
    // `Descend` step, so a postfix chain fuses onto it like any other path.
    let program = compiled("..");
    let steps = root_stage_steps(&program);
    assert_eq!(steps.len(), 1);
    assert!(matches!(steps[0].access(), StepAccess::Descend));
}

#[test]
fn pipe_of_paths_fuses_to_one_stage() {
    // `.a | .b` fuses to the same single stage as `.a.b`.
    let program = compiled(".a | .b");
    assert!(!program.program.split().is_whole_document());
    assert_eq!(program.program.split().prefix_len(), 2);
    assert_eq!(keys(&program), ["a", "b"]);
    assert_eq!(optional_flags(&program), [false, false]);
}

#[test]
fn pipe_chain_of_three_fuses_in_order() {
    let program = compiled(".a | .b | .c");
    assert_eq!(keys(&program), ["a", "b", "c"]);
}

#[test]
fn optional_flags_travel_with_fusion_across_a_pipe() {
    // `.a? | .b` flags the first fused step; `.a | .b?` the second.
    assert_eq!(optional_flags(&compiled(".a? | .b")), [true, false]);
    assert_eq!(optional_flags(&compiled(".a | .b?")), [false, true]);
}

#[test]
fn identity_pipe_identity_is_whole_document() {
    // Fusing two empty stages yields the empty stage: whole-document identity.
    let program = compiled(". | .");
    assert!(program.program.split().is_whole_document());
    assert!(root_stage_steps(&program).is_empty());
}

#[test]
fn pipe_of_paths_requirement_is_identical_to_the_fused_path() {
    // The fusion perf thesis: `.a | .b` produces a structurally identical
    // AccessRequirement to `.a.b` (account-independent structural equality).
    let piped = requirement_of(".a | .b");
    let fused = requirement_of(".a.b");
    assert_eq!(piped.footprint(), fused.footprint());
    assert_eq!(piped.result(), fused.result());
    assert_eq!(piped.footprint().fingerprint(), fused.footprint().fingerprint());
}

#[test]
fn optional_flags_do_not_alter_the_requirement() {
    // Optionality is interpretation, never access: `.a?.b`, `.a.b?`,
    // `.a? | .b`, and `.a.b` all lower to the identical requirement.
    let plain = requirement_of(".a.b");
    for source in [".a?.b", ".a.b?", ".a? | .b", ".a | .b?"] {
        let marked = requirement_of(source);
        assert_eq!(
            marked.footprint(),
            plain.footprint(),
            "{source} must not alter the requirement footprint"
        );
        assert_eq!(marked.result(), plain.result());
    }
}

#[test]
fn comma_lowers_to_a_choice_root() {
    // `.a, .b` is now in the subset: a top-level `Choice`, whole-document
    // pushdown (both members share one document authority).
    let program = compiled(".a, .b");
    assert!(program.program.split().is_whole_document());
    assert!(matches!(
        program.program.nodes()[program.program.root().index()],
        ProgramNode::Choice { .. }
    ));
}

#[test]
fn three_member_comma_is_left_associative() {
    // `.a, .b, .c` nests as `Choice(Choice(a, b), c)`: the root's left is a
    // Choice and its right is a Stage.
    let program = compiled(".a, .b, .c");
    let nodes = program.program.nodes();
    let ProgramNode::Choice { left, right } = &nodes[program.program.root().index()] else {
        panic!("comma root must be a Choice");
    };
    assert!(matches!(nodes[left.index()], ProgramNode::Choice { .. }));
    assert!(matches!(nodes[right.index()], ProgramNode::Stage { .. }));
}

#[test]
fn transparent_group_over_a_path_is_the_bare_stage() {
    // `(.a).b` and `((.a))` are transparent: groups produce no node, so the
    // postfix fuses onto the base stage exactly as `.a.b` / `.a` would.
    assert_eq!(keys(&compiled("(.a).b")), ["a", "b"]);
    assert_eq!(keys(&compiled("((.a))")), ["a"]);
    assert_eq!(keys(&compiled("(.a | .b).c")), ["a", "b", "c"]);
}

#[test]
fn group_postfix_on_a_choice_is_a_flatmap() {
    // `(.a, .b).c` composes the postfix `.c` onto a `Choice` base, which is
    // not a Stage, so it becomes `FlatMap(Choice, Stage[c])` — identical to
    // `(.a, .b) | .c`.
    let program = compiled("(.a, .b).c");
    let nodes = program.program.nodes();
    let ProgramNode::FlatMap { upstream, body } = &nodes[program.program.root().index()] else {
        panic!("group-postfix on a choice must be a FlatMap");
    };
    assert!(matches!(nodes[upstream.index()], ProgramNode::Choice { .. }));
    assert!(matches!(nodes[body.index()], ProgramNode::Stage { .. }));
    assert!(program.program.split().is_whole_document());
}

#[test]
fn pipe_into_a_group_choice_keeps_the_flatmap_and_pushes_the_prefix() {
    // `.a | (.b, .c)` keeps `FlatMap(Stage[a], Choice)`; the static prefix
    // `.a` sits in the FlatMap upstream and is still pushed down.
    let program = compiled(".a | (.b, .c)");
    let nodes = program.program.nodes();
    assert!(matches!(
        nodes[program.program.root().index()],
        ProgramNode::FlatMap { .. }
    ));
    assert!(!program.program.split().is_whole_document());
    assert_eq!(program.program.split().prefix_len(), 1);
}

#[test]
fn a_total_static_collect_body_hoists_its_whole_path() {
    // `[.a.b]` rewrites to `.a.b | [.]` — the same hoist the constructor gets
    // — so the path a bare collect could never contribute reaches the codec
    // as a pushdown prefix instead of costing the whole document to read one
    // element.
    for (source, prefix) in [("[.a]", 1), ("[.a.b]", 2), ("[.a[0].b]", 3), ("[.a | .b]", 2)] {
        let program = compiled(source);
        assert!(!program.program.split().is_whole_document(), "{source}");
        assert_eq!(program.program.split().prefix_len(), prefix, "{source}");
    }
    // The rewrite is all-or-nothing, so the residual is exactly `[.]`.
    let program = compiled("[.a.b]");
    let nodes = program.program.nodes();
    let ProgramNode::FlatMap { body, .. } = &nodes[program.program.root().index()] else {
        panic!("the hoist must leave a FlatMap root");
    };
    let ProgramNode::CollectArray { body: Some(body) } = &nodes[body.index()] else {
        panic!("the hoisted body must still be the collect");
    };
    let ProgramNode::Stage { steps, .. } = &nodes[body.index()] else {
        panic!("the collect residual must be a stage");
    };
    assert!(steps.is_empty(), "the residual is exactly `[.]`");
    // The declining default, each decline for its own law.
    for (source, law) in [
        ("[]", "no body to hoist"),
        ("[1]", "a literal-start body walks no path"),
        (
            "[.a?]",
            "an optional step may emit ZERO, and `[empty]` is `[]` while \
             `empty | [.]` publishes nothing at all",
        ),
        ("[.a, .b]", "a comma body has two producers and no single path"),
    ] {
        assert!(
            compiled(source).program.split().is_whole_document(),
            "{source} must decline the collect hoist ({law})"
        );
    }
}

#[test]
fn a_collect_barrier_is_transparent_to_the_pushdown_spine() {
    // `[f]` evaluates `f` against the collect's own input, so the barrier has
    // ONE producer whose dot IS the collect's — and the spine descends it
    // like a `FlatMap` upstream. This is the shape the all-or-nothing hoist
    // above declines (its residual would keep a suffix), and where the whole
    // 10k-element document used to be read to answer a one-key demand.
    for (source, prefix) in [
        ("[.a[]]", 1),
        ("[.a[] | .b]", 1),
        ("[.users[].id]", 1),
        ("[.a | length]", 1),
        ("[.a.b[].c] | sort", 2),
    ] {
        let program = compiled(source);
        assert!(!program.program.split().is_whole_document(), "{source}");
        assert_eq!(program.program.split().prefix_len(), prefix, "{source}");
    }
    // BENEATH the barrier the prefix stops at the first OPTIONAL step: a
    // pushed-down suppression publishes nothing, while `[.a.b?]` over
    // `{"a": 5}` is `[]`. `.a` still pushes down; the flagged step stays in
    // the residual, where the collect can see it emit nothing.
    assert_eq!(compiled("[.a.b?]").program.split().prefix_len(), 1);
    assert!(compiled("[.a?]").program.split().is_whole_document());
}

#[test]
fn a_spine_node_hoists_the_prefix_its_operands_share() {
    // The four multi-producer spine families read one dot through several
    // graphs, so what they can contribute is the prefix-lattice JOIN of
    // those graphs — the codec carries one exact path per requirement, so
    // their UNION is still not expressible and their join is.
    for (source, prefix) in [
        // Conditional: condition + the arm that runs, `null` exempt.
        ("if .a.b then .a.c else null end", 1),
        ("if .u[0].active then .u[0].email else null end", 2),
        ("if .a.b then .a.c else .a.d end", 1),
        // Alternative and the logical pair.
        (".a.b // .a.c", 1),
        (".a.b and .a.c", 1),
        (".a.b or .a.c", 1),
        // Binary, including operands whose leading stage sits under a pipe:
        // `.users | map(.age) | add` contributes `.users`.
        (".a.b + .a.c", 1),
        ("1 + .a.b", 2),
        // The join stops AT the first optional step rather than declining:
        // `.p` is total, and the `?` stays in the residual where a suppressed
        // step is still the alternative's own zero-output left operand.
        (".p.a? // 1", 1),
        ("(.users | map(.age) | add) / (.users | length)", 1),
        // The hoist COMPOSES: an inner spine node that hoisted its own join
        // became a `FlatMap` whose leading stage the outer join then reads,
        // so an `elif` ladder contributes the prefix its whole chain shares.
        ("if .a.b then .a.c elif .a.d then .a.e else .a.f end", 1),
        // A comma the Choice hoist rewrote is the same composition: the
        // then-arm is `.a | (.c, .d)`, so the outer join still sees `.a`.
        ("if .a.b then (.a.c, .a.d) else .a.e end", 1),
        // 014 Part A: an `==` operand joins like its `!=`/`<` siblings. The
        // probe-equality guard (`join::is_probe_equality`) protects only a
        // SELECT predicate's leftmost conjunct — the one context a scan row
        // can read — so a conditional condition, a logical/alternative/
        // binary operand, or a top-level comparison may hoist its shared
        // prefix freely.
        ("if .a.b == 1 then .a.c else .a.d end", 1),
        ("if (.a.b == 1 and .a.c > 0) then .a.d else .a.e end", 1),
        ("(.a.b == 1) // .a.c", 1),
        ("(.a.b == 1) and .a.c", 1),
        ("(.a.b == 1) or .a.c", 1),
        ("(.a.b == 1) + .a.c", 1),
        (".a.b == 1", 2),
    ] {
        let program = compiled(source);
        assert!(!program.program.split().is_whole_document(), "{source}");
        assert_eq!(program.program.split().prefix_len(), prefix, "{source}");
    }
}

#[test]
fn a_spine_hoist_declines_every_shape_it_cannot_prove() {
    for (source, law) in [
        // Operands that diverge at their first step have an empty join. The
        // demand-union plan wanted `{.a.b, .c, .d}` here; the codec's
        // footprint vocabulary has no union form, so this keeps the floor.
        ("if .a.b then .c else .d end", "an empty join is no prefix"),
        (
            "if (.a.b == 1) then .c else .d end",
            "an `==` operand joins no more than its bare sibling — the arms \
             still diverge at the first step",
        ),
        (".a // .b", "the same, for the alternative"),
        // The witness rule: only the condition is evaluated unconditionally,
        // so a literal condition cannot license a prefix the arms share —
        // the answer is `1` for this program over a scalar input.
        ("if true then 1 else .p.x end", "no always-evaluated witness"),
        (
            ".p?.a // 1",
            "a LEADING optional step may emit ZERO, so the join is empty",
        ),
        // Poisoned operands: anything that is neither an exempt literal nor
        // a graph with a `Current`-start leading stage.
        (
            "if length then .a.c else .a.d end",
            "a call has no leading stage, so the witness cannot walk a prefix",
        ),
        (
            "if .a.b then length else .a.d end",
            "a call ARM poisons the node just as a call condition does",
        ),
        (
            "if .a.b then (.a.c, 1) else .a.e end",
            "a comma fuse_choice declined (literal arm) has no leading stage",
        ),
        (
            "if .a.b then (try .a.c) else .a.d end",
            "a `try` operand keeps its whole-input authority",
        ),
        (
            ".k as $x | (.[$x].a + .[$x].b)",
            "a LEADING dynamic step is not one the codec resolves",
        ),
    ] {
        assert!(
            compiled(source).program.split().is_whole_document(),
            "{source} must decline the spine hoist ({law})"
        );
    }
}

#[test]
fn a_spine_hoist_truncates_a_join_longer_than_the_bound() {
    // The bound is `MAX_HOISTED_PREFIX` (8). These operands share NINE
    // steps; the ninth stays in the residual, where it was before the
    // hoist existed.
    let program = compiled(".a.b.c.d.e.f.g.h.i.j + .a.b.c.d.e.f.g.h.i.k");
    assert_eq!(program.program.split().prefix_len(), 8);
}

#[test]
fn group_choice_equivalence_shares_the_requirement() {
    // `(.a, .b).c` ≡ `(.a, .b) | .c`: identical requirement (both whole
    // document, both graph-rooted at the same FlatMap(Choice, Stage[c])).
    let postfix = requirement_of("(.a, .b).c");
    let piped = requirement_of("(.a, .b) | .c");
    assert_eq!(postfix.footprint(), piped.footprint());
    assert_eq!(postfix.result(), piped.result());
}

#[test]
fn alternative_hoist_shares_choice_fences_and_the_piped_spelling() {
    use super::Access;
    let hoisted = compiled(".a.b // .a.c");
    assert!(!hoisted.program.split().is_whole_document());
    assert_eq!(hoisted.program.split().prefix_len(), 1);
    assert_eq!(hoisted.access(), Access::Exact);
    let piped = compiled(".a | (.b // .c)");
    assert_eq!(piped.program.split().prefix_len(), 1);
    assert_eq!(
        requirement_of(".a.b // .a.c").footprint(),
        requirement_of(".a | (.b // .c)").footprint()
    );
    assert!(compiled(".a // .b").program.split().is_whole_document());
    assert!(compiled(".a?.b // .a?.c").program.split().is_whole_document());
    assert_eq!(compiled(".p.a? // 1").program.split().prefix_len(), 1);
}

#[test]
fn three_arm_comma_compiles_to_exact_of_the_shared_stem() {
    use super::Access;
    let hoisted = compiled(".a.b, .a.c, .a.d");
    let piped = compiled(".a | (.b, .c, .d)");
    assert_eq!(hoisted.access(), Access::Exact);
    assert_eq!(piped.access(), Access::Exact);
    let hoisted_req = requirement_of(".a.b, .a.c, .a.d");
    let piped_req = requirement_of(".a | (.b, .c, .d)");
    assert!(
        !hoisted_req.footprint().is_whole(),
        "three-arm comma must be Exact of the shared stem, not Whole"
    );
    let path = hoisted_req.footprint().exact_path().expect("Exact path");
    assert_eq!(path.steps(), &[PortableStep::SemanticMember("a".into())]);
    assert_eq!(hoisted_req.footprint(), piped_req.footprint());
}

#[test]
fn constructor_and_literal_bases_are_accepted() {
    // Postfix on a constructor or literal base now compiles (`[1,2][0]` → 1,
    // `{a: .b}.a`, `3[]?`): the array/object/literal bases join identity and
    // group as accepted postfix bases.
    for source in ["[1,2][0]", "{a: .b}.a", "3[]?", "(1).a?", "[.[]][0]"] {
        assert!(compile(source).is_ok(), "{source} must compile");
    }
}

#[test]
fn every_expression_is_a_postfix_base() {
    // FENCE RETIRED: the base position carries no law of its own — `BASE
    // STEPS` is `BASE | .STEPS` — so the last shapes the bootstrap called a
    // "non-identity base" now compose like every other base. `empty[0]`
    // yields nothing, `(reduce …)[0]` indexes the fold's result, and
    // `if … end []` iterates the taken branch (`if true then [.] else .
    // end []` over `null` is `null`).
    for source in [
        "empty[0]",
        "reduce .[] as $x (0; .)[0]",
        "foreach .[] as $x (0; .)[0]",
        "if true then [.] else . end []",
        "if true then [.] else . end[0]?",
    ] {
        assert!(compile(source).is_ok(), "{source} must compile");
    }
}

#[test]
fn a_base_that_did_not_parse_is_still_refused() {
    // …and the widening must not swallow a recovered parse. It cannot: the
    // parse rejection is raised BEFORE lowering runs, so a base whose
    // expression is an `ExprKind::Error` never reaches `lower_postfix_base`
    // at all. Pinned here because the widened base is exactly the place
    // where a silently-composed error node would be invisible.
    assert!(
        matches!(compile("(|)[0]"), Err(EngineCompileError::Parse(_))),
        "a malformed base must be a parse rejection"
    );
}

#[test]
fn a_format_filter_is_a_postfix_base() {
    // FENCE FLIP: `@text[0:1]` parses as a postfix chain on the format
    // filter, which is why `{"a":1} | @text.a` reports the INDEX refusal at
    // RUN time rather than failing to parse. The chain therefore has to
    // lower, and what it lowers to is a `format` call with the steps
    // composed after it.
    for source in ["@base64[0]", "@text[0:1]", "@text.a", "@base64 \"x\\(.)\"[0]"] {
        let program = compiled(source);
        assert!(
            matches!(
                program.program.nodes()[program.program.root().index()],
                ProgramNode::FlatMap { .. }
            ),
            "a format base with a postfix step lowers to a mapped chain: {source}"
        );
    }
}

#[test]
fn format_template_object_shorthand_looks_up_the_produced_key() {
    // `{@text "k"}` is object shorthand: the format produces the key, the
    // value is a lookup of that key on the input. On null input the
    // lookup is null, so the constructor emits `{"k":null}`.
    use crate::exec::{EngineRun, RunPoll};
    use jqf_builtins::codec_result::{CodecInputOutcome, EngineResult};
    let mut resources = resources();
    let compiled = compiled(r#"{@text "k"}"#);
    let mut stream = match compiled
        .try_run_whole_value(CodecInputOutcome::Result(EngineResult::owned(Value::Null)), &resources)
        .expect("run")
    {
        EngineRun::Stream { stream, .. } => stream,
        other => panic!("expected a stream: {other:?}"),
    };
    let item = match stream.poll(&mut resources) {
        Ok(RunPoll::Item(EngineResult::Owned(value))) => {
            jqf_builtins::semantics::render::to_json(&value).expect("json")
        }
        other => panic!("expected one owned object: {other:?}"),
    };
    assert_eq!(item, r#"{"k":null}"#);
    assert!(matches!(stream.poll(&mut resources), Ok(RunPoll::Complete)));
}

#[test]
fn group_term_suppression_lowers_to_a_catchless_try() {
    // FENCE FLIP: `(.a)?` and `(.a, .b)?` are a `?` on a whole GROUP term —
    // the whole-term `try` — now legal, a catchless `Try` over the group
    // body.
    for source in ["(.a)?", "(.a, .b)?"] {
        let program = compiled(source);
        assert!(
            matches!(
                program.program.nodes()[program.program.root().index()],
                ProgramNode::Try { handler: None, .. }
            ),
            "{source} lowers to a catchless Try"
        );
    }
}

#[test]
fn builtin_calls_compile() {
    // The seed builtins now resolve and compile (bare and parenthesized).
    for source in [
        "length",
        "keys",
        "select(.a)",
        "map(.id)",
        "map(.id)[0]",
        ".items | map(.id) | length",
        "not",
        "true | not",
    ] {
        assert!(compile(source).is_ok(), "{source} must compile");
    }
}

#[test]
fn control_flow_constructs_compile() {
    // The control-flow vertical: `if`/`elif`/`else` (and the else-less form),
    // `and`/`or`, and `//`. All were rejected before this vertical; each now
    // compiles to a graph-rooted program.
    for source in [
        "if .a then 1 else 2 end",
        "if .a then 1 end",
        "if .a then 1 elif .b then 2 else 3 end",
        ".a and .b",
        ".a or .b",
        ".a // .b",
        ".a // .b // .c",
        ".x | if . then 1 else 2 end",
    ] {
        let program = compile(source).expect("compiles");
        assert!(
            !matches!(
                program.program.nodes()[program.program.root().index()],
                ProgramNode::Stage { .. }
            ),
            "{source} lowers to a control-flow graph (never a bare Stage)"
        );
    }
}

#[test]
fn unknown_and_wrong_arity_calls_are_undefined() {
    for (source, name, arity) in [
        ("nonexistent", "nonexistent", 0),
        ("length(1)", "length", 1),
        ("map", "map", 0),
        ("select", "select", 0),
    ] {
        match compile(source) {
            Err(EngineCompileError::UndefinedCall {
                name: got_name,
                arity: got_arity,
                ..
            }) => {
                assert_eq!(got_name, name, "{source}");
                assert_eq!(got_arity, arity, "{source}");
            }
            other => panic!("expected {source:?} undefined, got {other:?}"),
        }
    }
}

#[test]
fn map_lowers_to_the_same_plan_as_the_bracket_pipe_expansion() {
    // `map(f)` ≡ `[.[] | f]`: identical requirement (both whole-document,
    // both graph-rooted at the same CollectArray). The Lowering IS the plan.
    let mapped = requirement_of("map(.id)");
    let expanded = requirement_of("[.[] | .id]");
    assert_eq!(mapped.footprint(), expanded.footprint());
    assert_eq!(mapped.result(), expanded.result());
}

#[test]
fn binding_and_loop_constructs_compile() {
    // The reduce/foreach + `as`-bindings vertical: bindings, both loop
    // arities, `empty`, bound-variable postfix navigation, and `.[$x]`.
    for source in [
        ". as $x | $x",
        ".[] as $x | [$x, $x * 2]",
        ". as $x | 2 as $x | $x",
        "reduce .[] as $x (0; . + $x)",
        "reduce .[] as $x (.k; . + $x)",
        "foreach .[] as $x (0; . + $x)",
        "foreach .[] as $x (0; . + $x; [$x, .])",
        "empty",
        "empty as $x | 1",
        "1 + reduce .[] as $x (0; . + $x)",
        "map(reduce .[] as $x (0; . + $x))",
        ".[] as $row | reduce $row[] as $y (0; . + $y)",
        ".a[0] as $r | $r.b[1]",
        "5 as $v | $v.a?",
        ".k as $x | .o[$x]",
        "1 as $i | .[$i]",
        "1 as $i | .[$i]?",
        "{a: (.b as $v | $v + 1)}",
    ] {
        assert!(compile(source).is_ok(), "{source} must compile");
    }
}

#[test]
fn a_bound_variable_lowers_to_a_variable_start_stage() {
    // `$x` is the third stage START (beside `Current` and `Literal`): the
    // executor reads the slot, never a name.
    let program = compiled(". as $x | $x");
    let nodes = program.program.nodes();
    let ProgramNode::Bind { body, slot, .. } = &nodes[program.program.root().index()] else {
        panic!("a binding lowers to a Bind node");
    };
    assert_eq!(*slot, 0, "the first binder occurrence owns slot 0");
    assert!(
        matches!(
            &nodes[body.index()],
            ProgramNode::Stage {
                start: StageStart::Variable(0),
                ..
            }
        ),
        "the body `$x` is a Variable-start stage"
    );
}

#[test]
fn each_binder_occurrence_owns_its_own_slot() {
    // Slots are per OCCURRENCE, never reused across sibling scopes —
    // `1 as $x | ...` and the nested `(10,20) as $x | ...` take
    // distinct slots even though both bind the name `$x`.
    let program = compiled("1 as $x | (((10,20) as $x | $x) | . + $x)");
    assert_eq!(program.program.slots(), 2, "two binder occurrences declare two slots");
}

#[test]
fn a_binder_free_program_declares_no_slots() {
    for source in [".", ".a[].b", ".a, .b", "map(.id)", "if .a then 1 else 2 end"] {
        assert_eq!(
            compiled(source).program.slots(),
            0,
            "{source} binds nothing, so it declares no slots"
        );
    }
}

#[test]
fn a_static_prefix_upstream_of_a_loop_still_pushes_down() {
    // The maximal-prefix pushdown law is unchanged by the binder families:
    // `.catalog | reduce .[] as $x (0; . + 1)` keeps the scoped route on its
    // prefix, and the requirement equals bare `.catalog`'s.
    for source in [
        ".catalog | reduce .[] as $x (0; . + 1)",
        ".catalog | foreach .[] as $x (0; . + 1)",
        ".catalog | (.[] as $x | $x)",
    ] {
        let program = compiled(source);
        assert!(!program.program.split().is_whole_document(), "{source}");
        assert_eq!(program.program.split().prefix_len(), 1, "{source}");
        assert_eq!(
            requirement_of(source).footprint(),
            requirement_of(".catalog").footprint(),
            "{source} must push exactly its static prefix down"
        );
    }
}

#[test]
fn a_destructuring_pattern_lowers_to_an_extraction_frame() {
    // Every pattern form compiles now, `?//` chains included. The one shape
    // still rejected is a chain in a THREE-argument `foreach`, and it keeps
    // its own named message rather than a generic parse failure.
    for source in [
        ".[] as [$a, $b] | $a",
        ". as {a: $v} | $v",
        ". as [[$a],{b:$c},$d] | [$a,$c,$d]",
        r#". as {$x, "y": $y, ("z"): $z, k: [$w]} | [$x,$y,$z,$w]"#,
        "reduce .[] as [$a,$b] (0; .+$a+$b)",
        "foreach .[] as {a:$v} (0; .+$v; [$v,.])",
        ".[] as [$a] ?// $a | $a",
        ".[] as {$a, b:[$c]} ?// [$a, {$b}] ?// $d | [$a,$b,$c,$d]",
        "reduce .[] as [$a] ?// $a (0; .+$a)",
        "foreach .[] as [$a] ?// $a (0; .+$a)",
    ] {
        assert!(compile(source).is_ok(), "{source} must compile");
    }
    assert_eq!(
        unsupported("foreach .[] as [$a] ?// $a (0; .+$a; .)"),
        UnsupportedConstruct::Expression("a `?//` destructuring alternative in a three-argument `foreach`")
    );
}

#[test]
fn a_nested_alternative_chain_is_linear_not_exponential() {
    // An arm USED to copy the body, so a chain in the body of a chain
    // squared, and `push_node` refused nests past 13-14. The body is now
    // lowered ONCE behind a `ChainBody` barrier, so a 20-deep nest compiles
    // with a LINEAR arena — the refusal is gone, not moved.
    let nest = |depth: usize| {
        let mut program = String::from(".");
        for _ in 0..depth {
            program.insert_str(0, ". as [$a] ?// $a | (");
            program.push(')');
        }
        program
    };
    // Depth 13 is the old refusal boundary's admission edge: the old
    // per-arm body copies put that nest somewhere between 100k and 200k
    // arena nodes (the bound refused 14). The shared body keeps the same
    // nest tiny, which is the linearity assertion. Deeper nests are exercised
    // through the CLI's big-stack request thread (jqf-cli/tests/
    // deep_program.rs), which is where the engine's no_std test binary
    // cannot follow.
    let outcome = compile(&nest(13)).expect("a nested chain must compile");
    assert!(
        outcome.program.nodes().len() < 20_000,
        "the arena must stay linear, not square: {} nodes",
        outcome.program.nodes().len()
    );
}

#[test]
fn an_alternative_chain_is_one_try_per_loser() {
    // `p1 ?// p2 ?// p3` is a Try chain over one shared matched value, so a
    // three-alternative chain has exactly two handlers — the last
    // alternative is the escape and gets none.
    let program = compiled(". as [$a] ?// {$a} ?// $a | $a");
    let handlers = program
        .program
        .nodes()
        .iter()
        .filter(|node| matches!(node, ProgramNode::Try { handler: Some(_), .. }))
        .count();
    assert_eq!(handlers, 2, "one handler per non-final alternative");
}

#[test]
fn every_alternative_binds_the_chains_whole_name_union() {
    // A name only one alternative mentions is still bound — to `null` — in
    // every other arm, which is why `1 | [. as [$a] ?// $b | [$a,$b]]` is
    // `[[null,1]]` and not an undefined-variable compile error.
    for source in [
        ". as [$a] ?// $b | [$a,$b]",
        ". as [$a,$c] ?// {$b} | [$a,$b,$c]",
        ". as {$a} ?// [$b] ?// $c | [$a,$b,$c]",
    ] {
        assert!(compile(source).is_ok(), "{source} must bind the union");
    }
}

#[test]
fn a_pattern_key_expression_cannot_see_the_patterns_own_variables() {
    // The scope discipline, and the reason the frame is built before any name
    // opens: `. as {k:$k, ($k):$v}` is a compile-time `$k is not defined`.
    assert!(matches!(
        compile(". as {k:$k, ($k):$v} | [$k,$v]"),
        Err(EngineCompileError::UndefinedVariable { .. })
    ));
}

#[test]
fn a_repeated_pattern_name_is_two_slots() {
    // `[1,2] | . as [$a,$a] | $a` is `2`: one slot per binder OCCURRENCE, with
    // the later one shadowing in the body.
    let program = compiled(". as [$a,$a] | $a");
    let nodes = program.program.nodes();
    let slots: Vec<_> = nodes
        .iter()
        .filter_map(|node| match node {
            ProgramNode::Bind { slot, .. } => Some(*slot),
            _ => None,
        })
        .collect();
    let mut unique = slots.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(slots.len(), unique.len(), "no binder may share a slot");
}

#[test]
fn env_lowers_and_loc_is_a_location_literal() {
    // `$ENV` lowers to the `env/0` read, so `$ENV == env` holds by
    // construction; `$__loc__` is the source-location binding and lowers
    // to the literal `{"file": "<top-level>", "line": N}` — the label is
    // the compile source's own, and the line counts from the program text
    // (the corpus pins both spellings).
    assert!(compile("$ENV").is_ok(), "$ENV must lower to the env read");
    let compiled = compile("$__loc__").expect("$__loc__ must lower");
    let root = compiled.root();
    let crate::program::ProgramNode::Stage {
        start: crate::program::StageStart::Literal(value),
        ..
    } = &compiled.arena()[root.index()]
    else {
        panic!("$__loc__ must lower to a literal stage");
    };
    let jqf_data::Value::Object(object) = value.untagged() else {
        panic!("$__loc__ must be an object");
    };
    let Value::String(file) = object.get("file").map(jqf_data::Value::untagged).expect("file") else {
        panic!("file must be a string");
    };
    assert_eq!(file, "<top-level>");
    assert!(matches!(
        object.get("line").map(jqf_data::Value::untagged),
        Some(Value::Number(_))
    ));
}

#[test]
fn compiles_label_and_break() {
    // The non-local-exit vertical replaced the rejection this test used to
    // assert. `break` resolves against its OWN scope stack, so a label and a
    // variable may share a spelling without either capturing the other.
    for source in [
        "label $out | 1",
        "label $out | (1, break $out, 2)",
        "label $o | label $o | break $o",
        "label $x | (1 as $x | break $x)",
        "[label $o | .[] | break $o]",
    ] {
        assert!(compile(source).is_ok(), "{source} should compile");
    }
}

#[test]
fn parallel_compiles_with_prelude_names_do_not_race() {
    // Cold-start cache init must tolerate concurrent compiles that hit the gate.
    extern crate std;
    use std::sync::{Arc, Barrier};
    use std::thread;

    const THREADS: usize = 8;
    let barrier = Arc::new(Barrier::new(THREADS));
    let mut handles = Vec::with_capacity(THREADS);
    for _ in 0..THREADS {
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            compile("any").expect("stdlib-gated compile succeeds");
        }));
    }
    for handle in handles {
        handle.join().expect("thread completes");
    }
}

#[test]
fn every_prelude_definition_is_named_in_the_gate() {
    // The token gate decides whether the prelude is parsed at all, so a
    // definition missing from `STDLIB_NAMES` would be silently unreachable:
    // its calls would report `not defined` even though the prelude defines
    // it. This pins the two together in the only direction that can break.
    for line in super::STDLIB_PRELUDE.lines() {
        let Some(rest) = line.strip_prefix("def ") else {
            continue;
        };
        let name: &str = rest
            .split(['(', ':'])
            .next()
            .expect("a definition line names a function")
            .trim();
        assert!(
            super::STDLIB_NAMES.contains(&name),
            "prelude defines `{name}` but STDLIB_NAMES omits it, \
             so the gate would never parse the prelude for it"
        );
    }
}

#[test]
fn every_extension_prelude_definition_is_named_in_the_gate() {
    // The extension prelude rides the same token gate as the stdlib
    // one, so a definition missing from `EXTENSION_NAMES` would be silently
    // unreachable: its calls would report `not defined`.
    for line in super::EXTENSION_PRELUDE.lines() {
        let Some(rest) = line.strip_prefix("def ") else {
            continue;
        };
        let name: &str = rest
            .split(['(', ':'])
            .next()
            .expect("a definition line names a function")
            .trim();
        assert!(
            super::EXTENSION_NAMES.contains(&name),
            "extension prelude defines `{name}` but EXTENSION_NAMES omits it, \
             so the gate would never parse the prelude for it"
        );
    }
}

#[test]
fn prelude_enumerated_matches_the_preludes() {
    // `builtins/0` appends `PRELUDE_ENUMERATED` to the registry's own
    // enumeration; the two must describe the SAME surface — the union of
    // the stdlib and extension preludes. Every def either prelude spells
    // must be listed, and every listed name (except the `empty/0` syntax
    // form) must be spelled by one of them.
    let mut prelude = alloc::collections::BTreeSet::new();
    for prelude_text in [super::STDLIB_PRELUDE, super::EXTENSION_PRELUDE] {
        for line in prelude_text.lines() {
            let Some(rest) = line.strip_prefix("def ") else {
                continue;
            };
            let head = rest.split(':').next().expect("a definition line has a colon");
            let name = head
                .split(['(', ';'])
                .next()
                .expect("a definition line names a function");
            let arity = if head.contains('(') {
                u8::try_from(head.split('(').nth(1).unwrap_or("").split(';').count()).unwrap_or(u8::MAX)
            } else {
                0
            };
            prelude.insert((String::from(name), arity));
        }
    }
    for (name, arity) in jqf_builtins::registry::PRELUDE_ENUMERATED {
        // `empty` is a syntax form, not a prelude def; the `~` engine
        // constructors are engine-surface syntax, not prelude defs — both
        // are enumerated for discoverability but live outside the prelude
        // text, so the two-list pin exempts them.
        if (*name, *arity) == ("empty", 0) || name.starts_with('~') {
            continue;
        }
        assert!(
            prelude.contains(&(String::from(*name), *arity)),
            "PRELUDE_ENUMERATED lists {name}/{arity} but the preludes do not define it; prelude={prelude:?}"
        );
    }
    for (name, arity) in &prelude {
        assert!(
            jqf_builtins::registry::PRELUDE_ENUMERATED.contains(&(name.as_str(), *arity)),
            "the preludes define {name}/{arity} but PRELUDE_ENUMERATED omits it"
        );
    }
}

#[test]
fn the_prelude_gate_admits_every_name_it_lists() {
    // The other direction is only a wasted parse, but a stale name is still
    // a maintenance smell worth catching.
    for name in super::STDLIB_NAMES {
        assert!(
            super::STDLIB_PRELUDE.contains(&alloc::format!("def {name}")),
            "STDLIB_NAMES lists `{name}`, which the prelude does not define"
        );
    }
    for name in super::EXTENSION_NAMES {
        assert!(
            super::EXTENSION_PRELUDE.contains(&alloc::format!("def {name}")),
            "EXTENSION_NAMES lists `{name}`, which the prelude does not define"
        );
    }
}

#[test]
fn provable_divergence_compiles_and_is_a_runtime_condition() {
    // The compile-time always-recurses refusal was a FALSE POSITIVE on
    // programs the floor completes without ever evaluating the divergent
    // call (`first(1, def f: f; f)` answers 1), so the check was dropped:
    // a diverging program is a run-time condition now, and the tail-round
    // counter raises the typed depth error instead of hanging
    // (see `a_divergent_recursion_terminates_with_the_depth_error`).
    // Every one of these previously-rejected programs compiles.
    for source in [
        "def f: f; f",
        "def f: (f); f",
        "def f: f, 1; f",
        "def f: f | 1; f",
        "def f: if . then f else f end; f",
        "def f: label $o | f; f",
        "def f(g): f(g); f(.)",
    ] {
        compile(source).unwrap_or_else(|error| panic!("{source:?} must compile: {error:?}"));
    }
}

#[test]
fn a_recursion_with_an_escape_is_not_called_divergent() {
    // The analysis must never claim divergence it cannot prove: each of
    // these has a path that avoids the self-call, so the divergence analysis
    // must NOT refuse them. Each now routes to the CALLABLE path (recursion
    // compiles and runs, bounded at run time) — `empty | f` is the
    // sharpest: the floor TERMINATES on it (exit 0, no output), so calling it
    // divergent would be a straightforward lie.
    for source in [
        "def r: if . > 3 then . else .+1|r end; r",
        "def f: if . then 1 else f end; f",
        "def f: empty | f; f",
        "def f: .[] | f; f",
        "def f: f + 1; f",
        // A PRODUCTIVE generator: `1, f` emits before it recurses, so
        // `[limit(3; f)]` is `[1,1,1]` — the definition terminates for
        // every consumer that stops asking.
        // Calling it non-terminating was a false proof: only the LEFT
        // operand of a comma is guaranteed to be reached, and reaching the
        // right one means the left already finished.
        "def f: 1, f; f",
        "def f: (1, f), 2; f",
    ] {
        assert!(compile(source).is_ok(), "{source} must compile via the callable path");
    }
}

#[test]
fn a_divergent_recursion_terminates_with_the_depth_error() {
    // A genuinely divergent program still terminates with a typed depth
    // error — never a hang and never a native stack overflow. `def f: f; f`
    // is a bare tail self-call, which the trampoline runs flat, so it is the
    // tail-round ceiling that stops it; a non-tail divergence (`[f]`) stops
    // at the callable-depth ceiling. The debug per-level cost of a nested
    // callable is tens of KiB, so the nested arm runs on a big thread
    // exactly as the deep decode/recursion tests do.
    extern crate std;
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(|| {
            let programs = ["def f: f; f", "def f: [f]; f"];
            for source in programs {
                let program = compiled(source);
                let mut resources = resources();
                let outcome = jqf_builtins::codec_result::CodecInputOutcome::Result(
                    jqf_builtins::codec_result::EngineResult::owned(Value::Null),
                );
                let crate::exec::EngineRun::Stream { mut stream, .. } =
                    program.execute(outcome, &mut resources).expect("run seeds")
                else {
                    panic!("a recursive def is a stream");
                };
                let mut raised = false;
                loop {
                    match stream.poll(&mut resources) {
                        Ok(crate::exec::RunPoll::Complete) => break,
                        Ok(crate::exec::RunPoll::Pending) => {
                            // The fixed 4096-credit meter is exhausted;
                            // open the next cooperative entry the way the
                            // CLI host does.
                            resources.try_begin_next_cooperative_entry(4096).expect("entry");
                        }
                        Ok(crate::exec::RunPoll::Item(_)) => {}
                        Err(crate::exec::EngineRunError::Codec(error)) => {
                            assert!(
                                matches!(
                                    error.kind(),
                                    jqf_codec_core::CodecFailureKind::Resource(
                                        jqf_resource::ResourceError::RecursionLimit { .. }
                                    )
                                ),
                                "{source} must raise the typed depth error, got {error:?}"
                            );
                            raised = true;
                            break;
                        }
                        Err(other) => {
                            panic!("{source} must raise RecursionLimit, got {other:?}")
                        }
                    }
                }
                assert!(raised, "{source} must raise, not complete");
            }
        })
        .expect("big-stack thread spawns")
        .join()
        .expect("big-stack thread does not panic");
}
#[test]
fn tail_recursive_def_compiles_with_tail_flag_set() {
    let program = compiled("def f: if . <= 0 then \"done\" else (. - 1) | f end; f");
    let nodes = program.program.nodes();
    let mut tail_count = 0usize;
    for node in nodes {
        if let ProgramNode::CallDef { tail, .. } = node
            && *tail
        {
            tail_count += 1;
        }
    }
    assert!(
        tail_count >= 1,
        "tail-recursive def must have at least one tail=self CallDef, got {tail_count}"
    );
}

#[test]
fn non_tail_recursive_def_leaves_tail_flag_false() {
    let program = compiled("def f: if . <= 0 then 0 else 1 + (. - 1 | f) end; f");
    let nodes = program.program.nodes();
    let mut tail_self = false;
    for node in nodes {
        if let ProgramNode::CallDef { tail, .. } = node
            && *tail
        {
            tail_self = true;
        }
    }
    assert!(!tail_self, "non-tail recursive def must not mark any CallDef as tail");
}

#[test]
fn comma_last_member_self_call_is_marked_tail() {
    // `def f: 1, f` — the self-call is the comma's LAST
    // member, whose outputs are the comma's final outputs, so the
    // trampoline may re-run the body per round instead of nesting one
    // level per emitted output (`limit(5000; f)` must never approach the
    // depth ceiling). The LEFT member is never tail.
    for source in ["def f: 1, f; [limit(3;f)]", "def f: ., ((.+1)|f); [limit(3;f)]"] {
        let program = compiled(source);
        let mut tail_self = false;
        for node in program.program.nodes() {
            if let ProgramNode::CallDef { tail, .. } = node
                && *tail
            {
                tail_self = true;
            }
        }
        assert!(tail_self, "{source} must mark its comma-last self-call tail");
    }
}

#[test]
fn pipe_left_self_call_is_marked_tail() {
    // `def nat: 1, (nat|.+1)` — the self-call is the pipe's LEFT member.
    // Not classic tail, but the trampoline is sound there: the body
    // re-evaluates into the same FlatMapBody the call's outputs would
    // enter, and the accumulated frames re-apply each level's `. + 1`
    // exactly once. `limit(700; nat)` must answer, not hit the ceiling.
    let program = compiled("def nat: 1, (nat|.+1); [limit(3;nat)]");
    let mut tail_self = false;
    for node in program.program.nodes() {
        if let ProgramNode::CallDef { tail, .. } = node
            && *tail
        {
            tail_self = true;
        }
    }
    assert!(tail_self, "a pipe-left self-call must be marked for the trampoline");
}

#[test]
fn tail_recursive_def_with_params_compiles() {
    let program = compiled("def f($n): if $n <= 0 then \"done\" else f($n-1) end; f(1000)");
    let nodes = program.program.nodes();
    let mut tail_count = 0usize;
    for node in nodes {
        if let ProgramNode::CallDef { tail, .. } = node
            && *tail
        {
            tail_count += 1;
        }
    }
    assert!(
        tail_count >= 1,
        "tail-recursive def with params must have at least one tail=self CallDef, got {tail_count}"
    );
}

#[test]
fn tail_recursive_filter_parameter_def_marks_tail() {
    let program = compiled("def f(n): f(n); f(1)");
    let nodes = program.program.nodes();
    let mut tail_count = 0usize;
    let mut calldefs = 0usize;
    let mut callfilters = 0usize;
    for node in nodes {
        match node {
            ProgramNode::CallDef { tail, .. } => {
                calldefs += 1;
                if *tail {
                    tail_count += 1;
                }
            }
            ProgramNode::CallFilter { .. } => callfilters += 1,
            _ => {}
        }
    }
    assert!(
        tail_count >= 1,
        "tail-recursive filter-parameter def must mark a tail CallDef; \
         calldefs={calldefs} tail={tail_count} callfilters={callfilters}"
    );
}
#[test]
fn an_unbound_label_is_an_undefined_label_error() {
    // A label's lexical extent is its BODY only, exactly as a binding's is,
    // so a `break` outside it is the `$*label-o is not defined` (exit-3).
    // It is reported on its OWN arm rather than as an undefined VARIABLE:
    // the two namespaces are distinct, and conflating them would misstate
    // which scope stack the lookup missed.
    for source in ["break $o", "(label $o | 1), break $o", "1 as $o | break $o"] {
        match compile(source) {
            Err(EngineCompileError::UndefinedLabel { name, .. }) => {
                assert_eq!(name, "$o", "{source}");
            }
            other => panic!("expected {source:?} undefined label, got {other:?}"),
        }
    }
}

#[test]
fn an_unbound_variable_is_an_undefined_variable_error() {
    // A binding's lexical extent is its BODY only, so a reference outside it
    // is the compile-time `$x is not defined` (exit-3 class), reported as
    // a resolution failure rather than an out-of-subset rejection.
    for source in ["$x", "(2 as $x | $x), $x", "reduce .[] as $x (0; 0) | $x"] {
        match compile(source) {
            Err(EngineCompileError::UndefinedVariable { name, .. }) => {
                assert_eq!(name, "$x", "{source}");
            }
            other => panic!("expected {source:?} undefined, got {other:?}"),
        }
    }
}

#[test]
fn a_loop_init_cannot_see_its_own_binding() {
    // The scope discipline: the SOURCE and the INIT are outside the binding
    // (the init sees the outer dot), only UPDATE and EXTRACT are inside it.
    assert!(matches!(
        compile("reduce .[] as $x ($x; .)"),
        Err(EngineCompileError::UndefinedVariable { .. })
    ));
    assert!(matches!(
        compile("reduce $x as $x (0; .)"),
        Err(EngineCompileError::UndefinedVariable { .. })
    ));
}

#[test]
fn an_index_operand_is_lowered_against_the_chain_input() {
    // The frame wraps the WHOLE chain and `Bind`'s source runs with dot
    // unchanged, so `.a[.b]` resolves `.b` against the chain's INPUT. Lowering
    // the operand inside the stage would have read `.a` instead.
    let program = compiled(".a[.b]");
    let nodes = program.program.nodes();
    let ProgramNode::Bind { source, body, .. } = &nodes[program.program.root().index()] else {
        panic!("a general index must lower to a binder frame");
    };
    let ProgramNode::Stage {
        start: StageStart::Current,
        steps: operand,
    } = &nodes[source.index()]
    else {
        panic!("the operand must start from the chain's own input");
    };
    assert!(matches!(operand[0].access(), StepAccess::Key(key) if key == "b"));
    let ProgramNode::Stage { steps: chain, .. } = &nodes[body.index()] else {
        panic!("the frame must wrap the chain's stage");
    };
    assert!(matches!(chain[0].access(), StepAccess::Key(key) if key == "a"));
    assert!(matches!(chain[1].access(), StepAccess::DynVar(_)));
}

#[test]
fn the_last_bracket_is_the_outermost_binder() {
    // The fork-creation order: the LATER a generator's forks are created the
    // faster it varies, so in `.[.i][.j]` the last bracket is the OUTER loop
    // (`[.[0,1][0,1]]` on `[[10,11],[20,21]]` is `[10,20,11,21]`). Structurally
    // that is the root frame owning the LAST step's slot.
    let program = compiled(".[.i][.j]");
    let nodes = program.program.nodes();
    let ProgramNode::Bind { slot: outer, body, .. } = &nodes[program.program.root().index()] else {
        panic!("two general indexes must nest two binder frames");
    };
    let ProgramNode::Bind {
        slot: inner,
        body: stage,
        ..
    } = &nodes[body.index()]
    else {
        panic!("the outer frame must wrap the inner one");
    };
    let ProgramNode::Stage { steps, .. } = &nodes[stage.index()] else {
        panic!("the inner frame must wrap the stage");
    };
    assert!(matches!(steps[0].access(), StepAccess::DynVar(bound) if bound == inner));
    assert!(matches!(steps[1].access(), StepAccess::DynVar(bound) if bound == outer));
}

#[test]
fn a_term_suppression_barrier_encloses_the_binders_it_covers() {
    // A TERM-level `?` covers the index operand's own error
    // (`[(.[error("x")])?]` is `[]`) while a per-component `?` does not
    // (`[.[error("x")]?]` raises, `[.[error("x")]??]` is `[]`). So the frame the
    // barrier's PREFIX accumulated is flushed INSIDE the `try` body.
    for source in ["(.[.a])?", ".[.a]??"] {
        let program = compiled(source);
        let nodes = program.program.nodes();
        let ProgramNode::Try { body, handler: None } = &nodes[program.program.root().index()] else {
            panic!("{source} must lower to a catchless try");
        };
        assert!(
            matches!(&nodes[body.index()], ProgramNode::Bind { .. }),
            "{source} must keep its binder frame inside the barrier"
        );
    }
}

/// The consumer of a named element boundary (fold, bind, collect, residual).
#[test]
fn the_boundary_is_named_inside_a_reduce_or_foreach_source() {
    // The leading shape: `PushdownSplit` could not name a `.[]` inside a
    // loop SOURCE. It can now — the prefix is `.catalog`, the boundary is
    // the `Each` after it, and the loop is the residual consumer of the
    // elements.
    for source in [
        "reduce .catalog[] as $x (0; . + 1)",
        "foreach .catalog[] as $x (0; . + 1)",
    ] {
        assert_eq!(
            compiled(source).element_boundary_consumer(),
            Some(BoundaryConsumer::Fold),
            "{source}"
        );
    }
    // A bound source (`SRC as $x | BODY`) is the binding flavor of the same
    // shape.
    assert_eq!(
        compiled("[.catalog[] as $x | $x.id]").element_boundary_consumer(),
        Some(BoundaryConsumer::Binding)
    );
}

#[test]
fn the_boundary_is_named_through_maps_lowered_body() {
    // `map(f)` lowers to `[.[] | f]`, so its `.[]` lives inside a
    // `CollectArray` body — the second place the pushdown prefix never
    // reaches. The container path splits across the pipe: `.catalog` comes
    // from the FlatMap upstream, the `.[]` from the collect body's stage.
    assert_eq!(
        compiled(".catalog | map(.name) | length").element_boundary_consumer(),
        Some(BoundaryConsumer::Collect)
    );
    assert_eq!(
        compiled("[.catalog[]] | length").element_boundary_consumer(),
        Some(BoundaryConsumer::Collect)
    );
}

#[test]
fn a_non_document_or_non_static_each_names_no_boundary() {
    // A literal-start producer's `.[]` is not a DOCUMENT element stream
    // (the earlier walk recorded one here).
    assert_eq!(compiled("3[]?").element_boundary_consumer(), None);
    // Born-P2 vocabulary before the `.[]` leaves no static container path.
    assert_eq!(compiled("reduce .. as $x (0; . + 1)").element_boundary_consumer(), None);
    assert_eq!(compiled("..[]").element_boundary_consumer(), None);
    // A comma has two members and therefore no single boundary.
    assert_eq!(compiled(".a[], .b[]").element_boundary_consumer(), None);
    // No `.[]` at all.
    assert_eq!(compiled(".catalog[0].id").element_boundary_consumer(), None);
}

#[test]
fn every_receipt_table_row_has_a_nameable_boundary() {
    // Every row: each one NAMES its element boundary — `.catalog` in every
    // case — and records what consumes the elements.
    let table = [
        ("reduce .catalog[] as $x (0; . + 1)", BoundaryConsumer::Fold),
        ("[.catalog[]] | length", BoundaryConsumer::Collect),
        (".catalog | map(.name) | length", BoundaryConsumer::Collect),
        ("reduce .catalog[].id as $i (0; . + $i)", BoundaryConsumer::Fold),
        ("[.catalog[] | select(.id > 35990)] | length", BoundaryConsumer::Collect),
        ("[.catalog[] | select(.id > 35990)]", BoundaryConsumer::Collect),
        ("[.catalog[] | length]", BoundaryConsumer::Collect),
    ];
    for (source, consumer) in table {
        let program = compiled(source);
        assert_eq!(
            program.element_boundary_consumer(),
            Some(consumer),
            "{source} must name a boundary with the {consumer:?} consumer"
        );
    }
}

#[test]
fn a_bind_or_loop_source_pushes_its_static_path_down() {
    // The pushdown spine now enters a `Bind`/`Reduce`/`Foreach` SOURCE when
    // every other outer-dot reader is document-independent AND the codec can
    // resolve the source completely. The requirement must then be
    // byte-identical to the bare source path's.
    let resources = resources();
    for (source, bare) in [
        (".catalog[500].id as $i | [$i, $i*2]", ".catalog[500].id"),
        (".catalog[0] as $x | $x.id", ".catalog[0]"),
        ("reduce .meta.n as $n (0; . + $n)", ".meta.n"),
        ("foreach .meta.n as $n (0; . + $n)", ".meta.n"),
        (".a.b as $x | {v: $x}", ".a.b"),
    ] {
        let pushed = compiled(source)
            .try_requirement(&resources)
            .expect("bind-source requirement");
        let plain = compiled(bare).try_requirement(&resources).expect("bare requirement");
        assert_eq!(pushed.result(), AccessResultKind::Located, "{source}");
        assert_eq!(
            pushed.footprint().fingerprint(),
            plain.footprint().fingerprint(),
            "{source} must lower {bare}'s footprint"
        );
    }

    // The three declining laws, each for its own reason.
    for source in [
        // The binder's BODY reads the outer dot, which the located value is not.
        ".catalog[500].id as $i | .meta",
        // The loop's INIT reads the outer dot.
        "reduce .meta.n as $n (.meta.n; . + $n)",
        // The source FANS OUT over the located container rather than
        // resolving completely.
        "reduce .catalog[].id as $i (0; . + $i)",
        "reduce .catalog[] as $x (0; . + 1)",
        ".catalog[] as $x | [$x]",
        // A non-static step in the source.
        ".catalog[0].id as $i | (.catalog[$i:2] as $x | [$x])",
        ".catalog[0:2] as $x | [$x]",
    ] {
        assert_eq!(
            compiled(source)
                .try_requirement(&resources)
                .expect("requirement")
                .result(),
            AccessResultKind::CompleteDocument,
            "{source} must keep the whole-document route"
        );
    }
}

#[test]
fn the_range_locate_row_is_the_bare_slice_publish_and_nothing_else() {
    // The row: a bare `Stage` of static Key/Index steps ending in exactly
    // ONE non-optional slice, with nothing after it.
    for source in [
        ".catalog[100:110]",
        ".catalog[:10]",
        ".catalog[10:]",
        ".[1:3]",
        ".a.b[1:3]",
        ".nested[1][0:2]",
        ".catalog[1.7:3.2]",
        // Negative bounds resolve against the observed length in the codec
        // so a tail slice is a row like any other.
        ".catalog[-3:]",
        ".catalog[:-2]",
        ".catalog[-5:-1]",
        // An optional PREFIX step is fine: it only changes the outcome on a
        // path the rung declines anyway.
        ".catalog?[1:3]",
    ] {
        assert!(
            compiled(source).host_io() == HostIo::SpanCut,
            "{source} must be range-locate eligible"
        );
    }
    for (source, why) in [
        // One trailing range; stacks decline.
        (".catalog[0:4][1:3]", "a slice STACK"),
        // Nothing may follow the slice — the codec's exact path cannot carry
        // it and the rung runs no residual.
        (".catalog[1:3][0]", "a step after the slice"),
        (".catalog[1:3].name", "a key after the slice"),
        (".catalog[1:3] | length", "the range-COUNT row, which is a pipe"),
        // Prefix-scoping: an element boundary, a descent, or a dynamic index
        // before the slice is not a static exact path.
        (".catalog[][0:2]", "an `.[]` before the slice"),
        ("..[0:2]", "a descent before the slice"),
        ("0 as $i | .catalog[$i][0:2]", "a `$var` index before the slice"),
        // The classifications must not move.
        ("[.catalog[] | .tags[1:3]]", "a per-ELEMENT slice"),
        ("[.catalog[1:3][].name]", "a range on a CONTAINER path"),
        // The slice's own `?` is a different program; the floor serves it.
        (".catalog[1:3]?", "the slice step's own `?`"),
        // The boundary law's declines carry straight through.
        ("1 as $x | .catalog[$x:3]", "a `$var` BOUND"),
        // Not a bare `Stage` root at all.
        ("[.catalog[1:3]]", "a construction barrier"),
        (".catalog[1:3], .meta", "a comma"),
    ] {
        assert!(
            compiled(source).host_io() != HostIo::SpanCut,
            "{source} must decline the range-locate rung ({why})"
        );
    }
}

#[test]
fn naming_a_boundary_is_still_not_eligibility_by_itself() {
    // The rung was widened, and NAMING a boundary is still not the same as
    // taking it: every shape below names one and declines, each for a law
    // of its own (clause 3, or an absent shape row).
    //
    // The SPLIT-path restriction is no longer among those laws.
    // `.catalog | map(.name) | length` names its container across a pipe and
    // now streams; what its split path costs is the right to INTERPRET a
    // container-negative outcome, which the split path withholds and
    // `split_container_paths_stream_but_decline_the_container` pins.
    for source in [
        "reduce .catalog[] as $x (0; . + 1)",
        "foreach .catalog[] as $x (0; . + 1)",
        "[.catalog[] as $x | $x.id]",
    ] {
        let program = compiled(source);
        assert!(
            program.element_boundary_consumer().is_some(),
            "{source} must name a boundary"
        );
    }
}

#[test]
fn parse_error_is_surfaced() {
    assert!(matches!(compile(".["), Err(EngineCompileError::Parse(_))));
}

#[test]
fn the_correlated_joins_are_recognized_end_to_end() {
    // The recognition table is unit-tested against hand-built arenas; this
    // test pins it to the LOWERING, so a change to how `map`, `select`, or
    // `as` lowers cannot silently empty the table.
    for (source, rows) in [
        (
            ".users[0:100] as $users | .orders as $orders | $users \
             | map(. as $u | {id: $u.id, order_count: \
             ($orders | map(select(.user_id == $u.id)) | length)})",
            1,
        ),
        (
            ".orders as $orders | .orders[0:200] | map(. as $a \
             | {id: $a.id, siblings: ([$orders[] \
             | select(.user_id == $a.user_id and .channel == $a.channel)] | length)})",
            1,
        ),
        (
            ".orders as $orders | .users[0:1000] | map(. as $u \
             | {id: $u.id, matching: ([$orders[] | select(.user_id == $u.id \
             and .channel == (if $u.active then \"web\" else \"partner\" end))] \
             | length)})",
            1,
        ),
        (
            ".users[0:500] as $users | .orders as $orders | .events as $events | $users \
             | map(. as $u | {id: $u.id, email: $u.email} \
             + {orders: ($orders | map(select(.user_id == $u.id)) | length), \
             recent_events: ($events \
             | map(select(.user_id == $u.id and .created_day < 30)) | length)})",
            2,
        ),
        // The K2 LITERAL probe, end to end. The row's own unit test builds
        // its arena by hand, and every row above probes through a `$var`,
        // so nothing here saw the shape until the spine hoist started
        // rewriting `Binary` nodes: `select(.id == 3)` would become
        // `.id | (. == 3)` and the row would vanish silently, with output
        // unchanged and the Θ(k·m) rescan back.
        (".orders as $o | [$o[] | select(.id == 3)] | length", 1),
        (".orders as $o | $o | map(select(.status == \"paid\")) | length", 1),
    ] {
        let program = compiled(source);
        assert_eq!(program.program.scans().len(), rows, "correlated-scan rows for {source}");
    }
}

#[test]
fn the_near_miss_join_shapes_stay_out_of_the_table() {
    // Shapes that LOOK correlated and are deliberate non-rows. (`INDEX/2`,
    // the other named non-row, is not a registered builtin here at all, so
    // the table cannot see it even in principle.)
    for source in [
        // The anti-join: expands through `all/2`'s label machinery.
        ".orders as $orders | [.users[0:1000][] | . as $u \
         | select(all($orders[]; .user_id != $u.id)) | .id]",
        // Not an equality.
        ".orders as $orders | .users[] | . as $u \
         | [$orders[] | select(.user_id != $u.id)] | length",
        // The leftmost conjunct of an `or` is not a necessary condition.
        ".orders as $orders | .users[] | . as $u \
         | [$orders[] | select(.user_id == $u.id or .other == $u.id)] | length",
        // Both sides read the current element: nothing is element-independent.
        ".orders as $orders | .users[] | . as $u \
         | [$orders[] | select(.user_id == .id)] | length",
        // A residual after the iteration: the value reaching `select` is not
        // the child, so the index's key path is not the predicate's.
        ".orders as $orders | .users[] | . as $u \
         | [$orders[].line | select(.user_id == $u.id)] | length",
    ] {
        assert!(compiled(source).program.scans().is_empty(), "must decline: {source}");
    }
    // The former "unbound source" near-miss — `.users[] | . as $u |
    // [.orders[] | select(.user_id == $u.id)]` — became a ROW with the
    // generalized source recognition: its inner scan is a `Current`-start
    // source (`[orders, Each]`), and the runtime warm-up keys the index
    // slot on the CONTAINER'S NODE HANDLE, so a container that is missing
    // (the scan declines to the naive raise), owned, or re-created per
    // outer iteration never builds an index it cannot amortize.
    let binding = compiled(".users[] | . as $u | [.orders[] | select(.user_id == $u.id)] | length");
    let scans = binding.program.scans();
    assert_eq!(scans.len(), 1, "the unbound-source spelling is a row now");
}

#[test]
fn finish_preserves_uses_inputs_cursor() {
    let program = compiled("~inputs as ~x | ~x.next");
    assert!(
        program.uses_inputs_cursor(),
        "a ~inputs binding must flag the inputs cursor"
    );
}

#[test]
fn finish_preserves_runtime_index_slot_for_split_exp() {
    let program =
        try_compile_program(r#""\($index)""#, policy(), CompileOptions::split_exp(), &resources()).expect("compiles");
    assert_eq!(
        program.runtime_index_slot(),
        Some(0),
        "split-exp compile must record the $index slot for seeding"
    );
}

#[test]
fn anti_join_rows_reach_the_program_cache() {
    let program = compiled(".o as $o | [.u[] | . as $x | select(all($o[]; .k != $x.k)) | .k]");
    assert_eq!(program.program.anti_joins().len(), 1);
}

#[test]
fn user_def_length_shadows_the_builtin() {
    use crate::exec::{EngineRun, RunPoll};
    use jqf_builtins::codec_result::{CodecInputOutcome, EngineResult};
    let mut resources = resources();
    let compiled = compiled("def length: 1; length");
    let mut stream = match compiled
        .try_run_whole_value(CodecInputOutcome::Result(EngineResult::owned(Value::Null)), &resources)
        .expect("run")
    {
        EngineRun::Stream { stream, .. } => stream,
        other => panic!("{other:?}"),
    };
    match stream.poll(&mut resources) {
        Ok(RunPoll::Item(EngineResult::Owned(value))) => {
            assert!(matches!(value.untagged(), jqf_data::Value::Number(n) if n.to_i64() == Some(1)));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn filter_parameter_map_shadows_the_map_lowering() {
    use crate::exec::{EngineRun, RunPoll};
    use jqf_builtins::codec_result::{CodecInputOutcome, EngineResult};
    let mut resources = resources();
    let compiled = compiled("def f(map): map; f(1)");
    let mut stream = match compiled
        .try_run_whole_value(CodecInputOutcome::Result(EngineResult::owned(Value::Null)), &resources)
        .expect("run")
    {
        EngineRun::Stream { stream, .. } => stream,
        other => panic!("{other:?}"),
    };
    match stream.poll(&mut resources) {
        Ok(RunPoll::Item(EngineResult::Owned(value))) => {
            assert!(matches!(value.untagged(), jqf_data::Value::Number(n) if n.to_i64() == Some(1)));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn split_exp_with_cli_vars_is_a_compile_error() {
    let args = [(String::from("$n"), Value::Bool(true))];
    let error = try_compile_program(
        "$index",
        policy(),
        CompileOptions {
            cli_vars: &args,
            split_exp: true,
            source_label: "<top-level>",
        },
        &resources(),
    )
    .expect_err("split_exp cannot carry CLI bindings");
    assert!(matches!(error, EngineCompileError::SplitExpWithCliVars));
    assert!(format!("{error}").contains("`--split-exp`"));
}

#[test]
fn flatmap_free_programs_leave_scan_tables_empty() {
    for source in [".", ".a", "1", "def f: 1; f"] {
        let program = compiled(source);
        assert!(
            program.program.scans().is_empty(),
            "{source}: correlated scans must stay empty"
        );
        assert!(
            program.program.anti_joins().is_empty(),
            "{source}: anti joins must stay empty"
        );
        assert!(
            program.program.topk_sorts().is_empty(),
            "{source}: topk sorts must stay empty"
        );
        assert!(
            !program.program.has_reachable_flatmap(),
            "{source}: live graph must be FlatMap-free"
        );
    }
    assert!(
        compiled("[.[] | select(.a)]").program.has_reachable_flatmap(),
        "a select scan must cache a reachable FlatMap"
    );
}

#[test]
fn callable_each_body_blocks_count_collect_fusion() {
    // Pre-fusion `graph_has_each_step` must walk the callable body, not treat
    // `CallDef.body` as an arena node id — otherwise `.[]` inside the def is
    // invisible and `[f]|length` would incorrectly fuse to CountCollect.
    let program = compiled("def f: .[]; [f] | length");
    assert!(
        !program
            .arena()
            .iter()
            .any(|node| matches!(node, ProgramNode::CountCollect { .. })),
        "a callable body containing .[] must keep FlatMap|length, not CountCollect"
    );
}

#[test]
fn foreach_destructuring_performs_one_fold_step_per_binding_set() {
    use crate::exec::{EngineRun, RunPoll};
    use jqf_builtins::codec_result::{CodecInputOutcome, EngineResult};
    let program = compiled(r#"[{"a":1,"b":2}] | [foreach .[] as {("a","b"):$x} (0; .+$x)]"#);
    let mut resources = resources();
    let mut stream = match program
        .try_run_whole_value(CodecInputOutcome::Result(EngineResult::owned(Value::Null)), &resources)
        .expect("run")
    {
        EngineRun::Stream { stream, .. } => stream,
        other => panic!("unexpected run shape: {other:?}"),
    };
    let first = match stream.poll(&mut resources).expect("poll") {
        RunPoll::Item(EngineResult::Owned(value)) => value,
        other => panic!("unexpected poll: {other:?}"),
    };
    let Value::Array(items) = first else {
        panic!("expected array result");
    };
    assert_eq!(items.len(), 2, "two binding sets must perform two fold steps");
    let Value::Number(first) = items.get(0).expect("first element") else {
        panic!("expected numeric fold output");
    };
    let Value::Number(second) = items.get(1).expect("second element") else {
        panic!("expected numeric fold output");
    };
    assert_eq!(
        first.to_i64(),
        Some(1),
        "first fold step accumulates the first binding set"
    );
    assert_eq!(
        second.to_i64(),
        Some(3),
        "second fold step sees the updated accumulator"
    );
}

#[test]
fn term_try_suffix_composes_outside_the_catchless_try() {
    let program = compiled(".a??.b");
    let nodes = program.arena();
    let has_catchless_try = nodes
        .iter()
        .any(|node| matches!(node, ProgramNode::Try { handler: None, .. }));
    let has_suffix_key = nodes.iter().any(|node| {
        matches!(
            node,
            ProgramNode::Stage {
                steps,
                ..
            } if steps.iter().any(|step| matches!(step.access(), StepAccess::Key(key) if key == "b"))
        )
    });
    assert!(has_catchless_try, "term suppression must wrap the `.a??` prefix");
    assert!(has_suffix_key, "the `.b` suffix must lower as its own stage step");
    assert!(
        !matches!(nodes[program.root().index()], ProgramNode::Try { .. }),
        "the suffix must sit outside the catchless Try root"
    );
}
