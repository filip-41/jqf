//! Expression dispatch: one arm per recognized syntax family.
//!
//! [`lower_expr`] / [`lower_expr_at_depth`], plus loc/env bindings and `$x`
//! resolution helpers used from the dispatch table and postfix.

#[allow(clippy::wildcard_imports)]
use super::*;

/// Lowers one expression into the arena, returning its root node id.
///
/// Each recognized syntax family has an arm; a form outside that surface is
/// rejected by name and span.
pub(crate) fn lower_expr<'ast>(
    expr: &'ast Expr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let limit = lowerer.resources.limits().max_nesting_depth();
    if lowerer.depth >= limit {
        return Err(EngineCompileError::NestingTooDeep {
            span: expr.span(),
            limit,
        });
    }
    lowerer.depth += 1;
    let node = lower_expr_at_depth(expr, source, lowerer);
    lowerer.depth -= 1;
    node
}

#[allow(
    clippy::too_many_lines,
    reason = "one arm per recognized syntax family: the lowering surface is read as a single table"
)]
pub(crate) fn lower_expr_at_depth<'ast>(
    expr: &'ast Expr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    match expr.kind() {
        ExprKind::Identity => push_node(&mut lowerer.nodes, current_stage()),
        // `..`: one `Descend` step on a current-start stage, so a postfix chain
        // (`..[0]?`, `..[]?`) fuses onto the same stage the way `.a[]` does, and
        // `.a | ..` fuses across the pipe like any other step pair.
        ExprKind::RecursiveDescent => push_node(
            &mut lowerer.nodes,
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: descend_steps()?,
            },
        ),
        // Groups are transparent: `(expr)` and `((expr))` lower to `expr`'s
        // graph, producing no node of their own.
        ExprKind::Group { expression, .. } => lower_expr(expression, source, lowerer),
        ExprKind::Postfix(postfix) => lower_postfix_expr(postfix, source, lowerer),
        // Scalar literals lower to a `Literal`-start stage: an owned scalar
        // producer that ignores its input (NO interpolation, NO `@format`).
        ExprKind::Null => push_node(&mut lowerer.nodes, literal_stage(Value::Null)),
        ExprKind::Bool(value) => push_node(&mut lowerer.nodes, literal_stage(Value::Bool(*value))),
        ExprKind::Number => push_node(
            &mut lowerer.nodes,
            literal_stage(lower_number(expr.span(), false, source)?),
        ),
        ExprKind::String(template) => lower_string_template(template, source, lowerer),
        // `@name` and `@name "…"`: both are `format("name")`, applied to the
        // input or to each hole. The parser does the same rewrite.
        ExprKind::Format => lower_format(expr.span(), source, lowerer),
        ExprKind::FormatTemplate { format, template } => {
            lower_format_template(format.span(), template, source, lowerer)
        }
        // `empty`: the zero-cardinality producer, unless a user `empty/0` or
        // a filter parameter named `empty` is visible — then the syntax form
        // is that call. Bare `true`/`false`/`null` never consult the def stack.
        ExprKind::Empty => {
            if let Some(inlined) = try_lower_zero_arg_user("empty", expr.span(), source, lowerer) {
                return inlined;
            }
            push_node(&mut lowerer.nodes, ProgramNode::Empty)
        }
        // `$x`: resolved against the lexical scope stack into a `Variable`-start
        // stage. The executor never sees the name.
        ExprKind::Variable => {
            // `$ENV` is the environment binding, and it lowers to the `env/0`
            // call so the two can never disagree (`$ENV == env` is `true`). It
            // bypasses `lower_user_call` deliberately: a `def env: …;` shadows
            // the builtin NAME for ordinary calls, but the `$ENV` binding is
            // pre-bound and unaffected by such a def.
            let name = source
                .text()
                .get(expr.span().range())
                .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("variable span out of range")))?;
            if name == "$ENV" {
                return lower_env_call(lowerer);
            }
            // A CLI binding (`--arg`/`--argjson`) satisfies a `$x` reference
            // only after every lexical scope has been consulted: `1 as $x | $x`
            // answers the BINDER, never the `--arg` value. Later CLI bindings
            // shadow earlier ones (the last-wins law). The binding lowers to a
            // literal producer exactly like a data
            // import's `$alias` — a `--arg` value is immutable, so every
            // reference shares one owned value.
            if let Some(node) = try_lower_loc_binding(expr.span(), source, lowerer)? {
                return Ok(node);
            }
            match resolve_variable(expr.span(), source, &lowerer.scopes) {
                Ok(slot) => push_node(&mut lowerer.nodes, variable_stage(slot)),
                Err(EngineCompileError::UndefinedVariable { span, name }) => {
                    // A data import binds `$alias` to the module's data array:
                    // the variable lowers to a literal producer, so every
                    // reference shares the same owned value. The alias sits in
                    // the SAME fallback tier as the CLI bindings — an OUTER
                    // binding of last resort, never a pre-emptor — so a local
                    // binder shadows it the same way a local binder shadows a
                    // data-import alias (`import "d" as $d; 1 as $d | $d`
                    // answers `1`). Later aliases shadow earlier ones.
                    match lowerer.module_vars.iter().rposition(|(bound, _)| bound == &name) {
                        Some(position) => {
                            let value = lowerer.module_vars[position].1.clone();
                            push_node(&mut lowerer.nodes, literal_stage(value))
                        }
                        None => match lowerer.cli_vars.iter().rev().find(|(bound, _)| bound == &name) {
                            Some((_, value)) => {
                                let value = value.clone();
                                push_node(&mut lowerer.nodes, literal_stage(value))
                            }
                            // The SPLIT lane's `$index`: an
                            // unbound `$index` under the split compile entry
                            // resolves to a runtime variable slot the split drive
                            // seeds per item with the item counter. The slot is
                            // allocated anonymously so no user-visible binder can
                            // capture it, and recorded once so the drive knows
                            // which slot to seed.
                            None if lowerer.runtime_index && name == "$index" => {
                                let slot = if let Some(slot) = lowerer.runtime_index_slot {
                                    slot
                                } else {
                                    let slot = lowerer.scopes.allocate_anonymous()?;
                                    lowerer.runtime_index_slot = Some(slot);
                                    slot
                                };
                                push_node(&mut lowerer.nodes, variable_stage(slot))
                            }
                            None => Err(EngineCompileError::undefined_variable(span, &name)),
                        },
                    }
                }
                Err(error) => Err(error),
            }
        }
        // A negated numeric literal (`-0`, `-5`, `-1.50`): the sign is a separate
        // token, so it arrives as a unary negate of a `Number` term, and the
        // whole term folds to ONE literal producer at compile time — which is
        // what keeps a negative literal a `Literal`-start stage for every landed
        // analysis fact. The fold runs the SAME value law the operator does, so
        // `-0.0` folds to `0.0`, rather than to the
        // text `-0.0`.
        ExprKind::Unary(unary) if unary.op == UnaryOp::Negate && matches!(unary.expr.kind(), ExprKind::Number) => {
            let magnitude = lower_number(unary.expr.span(), false, source)?;
            push_node(&mut lowerer.nodes, literal_stage(negated_literal(magnitude)?))
        }
        // Unary minus over anything else: `_negate` applied to the operand's
        // outputs, one per output. It is a registered builtin and NOT the
        // subtraction `0 - expr` — the refusal names one operand
        // (`string ("a") cannot be negated`) where a subtraction would name two,
        // and the negation preserves a number's spelling where the double
        // arithmetic does not (`-1.500` versus `0 - 1.500` → `-1.5`).
        //
        // The operand is one PREFIX expression, which the parser already
        // enforces, so `- 2 * 3 + 1` is `((-2) * 3) + 1` and `- null // 2`
        // raises rather than yielding 2.
        ExprKind::Unary(unary) if unary.op == UnaryOp::Negate => {
            let operand = lower_expr(&unary.expr, source, lowerer)?;
            let negate = builtin_call("_negate", &[], lowerer)?;
            push_node(
                &mut lowerer.nodes,
                ProgramNode::FlatMap {
                    upstream: operand,
                    body: negate,
                },
            )
        }
        // Array construction `[body?]`: one optional generator body collected into
        // an owned array (AGENTS.md — not a syntax vector of elements). An
        // all-constant body (including the comma sequence `[1,2]`) folds to ONE
        // literal producer at compile time, the constant-array fold.
        ExprKind::Array { expression, .. } => {
            if let Some(value) = try_fold_constant_array(expression.as_deref(), source)? {
                return push_node(&mut lowerer.nodes, literal_stage(value));
            }
            let body = match expression {
                Some(body) => Some(lower_expr(body, source, lowerer)?),
                None => None,
            };
            push_node(&mut lowerer.nodes, ProgramNode::CollectArray { body })
        }
        // Object construction `{members}`: member commas are contextual separators,
        // never `Choice`; each member is a key producer paired with a value one. An
        // object whose every member is constant folds to ONE literal producer at
        // compile time, the constant-object fold.
        ExprKind::Object { members, .. } => {
            if let Some(value) = try_fold_constant_object(members, source)? {
                return push_node(&mut lowerer.nodes, literal_stage(value));
            }
            let mut member_nodes = Vec::new();
            member_nodes
                .try_reserve(members.len())
                .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
            for member in members {
                member_nodes.push(lower_object_member(member, source, lowerer)?);
            }
            push_node(
                &mut lowerer.nodes,
                ProgramNode::ConstructObject { members: member_nodes },
            )
        }
        ExprKind::Binary(binary) if binary.op == BinaryOp::Pipe => {
            // `.` is `|`'s unit, so `. | f` and `f | .` ARE `f` — and dropping
            // the unit buys far more than a node. A bare identity stage is a
            // whole-document requirement by construction (see
            // `identity_lowers_through_root_requirement`), so leaving one on
            // the spine defeats every demand projection downstream of it:
            // `. | .users | length` decodes the whole document where
            // `.users | length` decodes one member. Folding here rather than
            // on the lowered graph keeps the arena free of orphans.
            if is_identity(&binary.left) {
                return lower_expr(&binary.right, source, lowerer);
            }
            if is_identity(&binary.right) {
                return lower_expr(&binary.left, source, lowerer);
            }
            let upstream = lower_expr(&binary.left, source, lowerer)?;
            let body = lower_expr(&binary.right, source, lowerer)?;
            // `[f] | length` fuses to a COUNT-ONLY collect: the collect body
            // runs exactly as
            // it would under `CollectArray`, but the frame counts its outputs
            // instead of materializing an owned array — `length` of the
            // constructed array IS its element count, and the collect
            // publishes nothing before completion, so the two spellings are
            // indistinguishable to every consumer. The graph-level check
            // cannot misfire on a user `def length`: a shadowing definition
            // was already inlined by `lower_call` and the body is not a
            // `Call` to the LENGTH overload.
            let fused = match (&lowerer.nodes[upstream.index()], &lowerer.nodes[body.index()]) {
                (ProgramNode::CollectArray { body }, ProgramNode::Call { overload, args, .. })
                    if overload.get() == jqf_builtins::registry::builtins::id::LENGTH && args.is_empty() =>
                {
                    Some(*body)
                }
                _ => None,
            };
            if let Some(collect_body) = fused {
                // The gate: a body containing a `.[]` iteration is a P0/P1
                // count/projection ROW (a static container boundary), and the
                // count demand answers it from the lazy document's span
                // skeleton WITHOUT decoding the document — fusing would trade
                // that answer for a whole decode-and-count. The descent
                // (`..`), generator, and dynamic-index bodies have no `Each`
                // step and the fusion is their only fast path.
                if let Some(collect_body) = collect_body
                    && !graph_has_each_step(&lowerer.nodes, &lowerer.callables, collect_body)
                {
                    return push_node(
                        &mut lowerer.nodes,
                        ProgramNode::CountCollect {
                            body: Some(collect_body),
                        },
                    );
                }
            }
            push_node(&mut lowerer.nodes, ProgramNode::FlatMap { upstream, body })
        }
        // Comma is executable choice: evaluate the complete left filter, then the
        // complete right filter, over the same input. Left-associative already in
        // the parser tree, so `a, b, c` nests as `Choice(Choice(a, b), c)`.
        ExprKind::Binary(binary) if binary.op == BinaryOp::Comma => {
            let left = lower_expr(&binary.left, source, lowerer)?;
            let right = lower_expr(&binary.right, source, lowerer)?;
            push_node(&mut lowerer.nodes, ProgramNode::Choice { left, right })
        }
        // Arithmetic (`+ - * / %`) and comparison (`== != < <= > >=`): the
        // right-outer Cartesian `Binary` node. `and`/`or`/`//` are NOT lowered
        // here — they have their own families below.
        ExprKind::Binary(binary) if arithmetic_binary_kind(binary.op).is_some() => {
            let op = arithmetic_binary_kind(binary.op)
                .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("binary op lost its kind")))?;
            let left = lower_expr(&binary.left, source, lowerer)?;
            let right = lower_expr(&binary.right, source, lowerer)?;
            push_node(&mut lowerer.nodes, ProgramNode::binary(op, left, right))
        }
        // Short-circuiting `and`/`or`: the LEFT-outer `Logical` family, separate
        // from the RIGHT-outer arithmetic `Binary` (their drive orders differ).
        ExprKind::Binary(binary) if logical_operator(binary.op).is_some() => {
            let operator = logical_operator(binary.op)
                .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("logical op lost its kind")))?;
            let left = lower_expr(&binary.left, source, lowerer)?;
            let right = lower_expr(&binary.right, source, lowerer)?;
            push_node(&mut lowerer.nodes, ProgramNode::Logical { operator, left, right })
        }
        // Alternative `left // right`: the filtering-pass-through node. Only the
        // left is truthiness-filtered; the right is the unfiltered fallback.
        ExprKind::Binary(binary) if binary.op == BinaryOp::Alternative => {
            let left = lower_expr(&binary.left, source, lowerer)?;
            let right = lower_expr(&binary.right, source, lowerer)?;
            push_node(&mut lowerer.nodes, ProgramNode::Alternative { left, right })
        }
        // `if C then A elif C2 then B else D end`: the branch vector desugars to
        // nested `Conditional`s and a missing `else` synthesizes an identity arm.
        ExprKind::If(conditional) => lower_conditional(conditional, source, lowerer),
        // `try body` / `try body catch handler`: the error barrier. A missing
        // `catch` is a catchless (native, no-handler) `Try` that swallows.
        ExprKind::Try(try_expr) => {
            let body = lower_expr(&try_expr.expr, source, lowerer)?;
            let handler = match &try_expr.handler {
                Some(handler) => Some(lower_expr(handler, source, lowerer)?),
                None => None,
            };
            push_node(&mut lowerer.nodes, ProgramNode::Try { body, handler })
        }
        // A BARE engine term `~name`: either a `~x` ENGINE BINDING reference in
        // value position (rejected — the value-return guard) or a bare engine
        // constructor reference. The only expressions whose base is an engine
        // binding are `~x.next`/`~x.rest`, handled in `lower_postfix_expr`;
        // everything else that reaches here crosses the value/engine boundary.
        ExprKind::EngineTerm { tilde_span, name } => {
            let name_text = source.text().get(name.range()).ok_or_else(|| {
                EngineCompileError::Parse(ParseRejection::internal("engine term name span out of range"))
            })?;
            let full = alloc::format!("~{name_text}");
            let span = tilde_span.merge(*name);
            match lowerer.engine_scopes.resolve(name_text) {
                // In scope: the binding is being used as a VALUE, which the
                // engine boundary forbids by construction.
                Some(_) => Err(EngineCompileError::engine_binding_as_value(span, &full)),
                // A bare constructor reference with no call — the same
                // boundary violation, but naming the constructor makes the
                // message actionable.
                None if is_engine_constructor(name_text) => Err(EngineCompileError::EngineBindingShape {
                    span,
                    reason: constructor_must_bind(name_text),
                }),
                None => Err(EngineCompileError::undefined_engine_binding(span, &full)),
            }
        }
        // An ENGINE-constructor call `~name(...)` in VALUE position: it must
        // be the value of an `as ~x` binder (`lower_binding` consumes it there);
        // anywhere else the cursor the constructor would build can never be
        // pulled, so the form is rejected at lower time.
        ExprKind::EngineCall { call, .. } => {
            let name_text = source.text().get(call.name.range()).ok_or_else(|| {
                EngineCompileError::Parse(ParseRejection::internal("engine call name span out of range"))
            })?;
            Err(EngineCompileError::EngineBindingShape {
                span: expr.span(),
                reason: constructor_must_bind(name_text),
            })
        }
        // `SOURCE as $x | BODY`: the source is lowered OUTSIDE the new scope (it
        // cannot see its own binding), then the scope opens for the body alone.
        ExprKind::Binding(binding) => lower_binding(binding, source, lowerer),
        // `reduce`/`foreach SOURCE as $x (INIT; UPDATE[; EXTRACT])`: the source
        // and the INIT are lowered outside the new scope (the init sees the
        // outer dot and not `$x`); UPDATE and EXTRACT are inside it.
        ExprKind::Reduce(loop_expr) => lower_loop(loop_expr, source, lowerer, false),
        ExprKind::Foreach(loop_expr) => lower_loop(loop_expr, source, lowerer, true),
        // `label $out | BODY`: the label scopes over the body ALONE, exactly as a
        // binding scopes over its body — `(label $o | 1), break $o` is a compile
        // error for the same reason `(2 as $x | $x), $x` is.
        ExprKind::Label { label, body, .. } => lower_label(*label, body, source, lowerer),
        // `def name(params): body; rest` — the definition scopes over `rest`, and
        // its body is lowered at each CALL SITE rather than once here.
        ExprKind::Definition(definition) => lower_definition(definition, source, lowerer),
        // The eight assignment operators: one `Modify` drive each, differing only
        // in what produces the new value and whether the right-hand side is bound
        // outside the fold.
        ExprKind::Assignment(assignment) => lower_assignment(assignment, source, lowerer),
        // `break $out`: resolved against the label stack, never the variable one.
        ExprKind::Break { label, .. } => {
            let slot = resolve_label(*label, source, &lowerer.labels)?;
            push_node(&mut lowerer.nodes, ProgramNode::Break { slot })
        }
        // A builtin call: resolve `(name, arity)` against the registry FIRST, then
        // lower its arguments (an `Evaluator` into a `Call` node, a `Lowering` by
        // expanding). An unresolved `(name, arity)` is the `name/arity is not
        // defined` compile error.
        ExprKind::Call(call) => lower_call(call, source, lowerer),
        ExprKind::Error => Err(crate::compile::parse::recovered_syntax_error()),
        ExprKind::Unary(_) | ExprKind::Binary(_) => Err(EngineCompileError::unsupported(
            expr.span(),
            describe_expr_kind(expr.kind()),
        )),
    }
}

/// Whether a variable reference names the `$__loc__` location binding.
pub(crate) fn is_loc_binding(span: Span, source: &SyntaxSource<'_>) -> bool {
    source.text().get(span.range()).is_some_and(|name| name == "$__loc__")
}

/// Whether `span` names the `$ENV` environment binding.
///
/// `$ENV` is not an ordinary binder slot: it lowers to the `env/0` call
/// everywhere it appears — a bare `$ENV`, `{$ENV}`, `.[$ENV]` and `.[$ENV:]` —
/// so the three sites that resolve variables directly must agree with the
/// lowered path.
pub(crate) fn is_env_variable(span: Span, source: &SyntaxSource<'_>) -> bool {
    source.text().get(span.range()).is_some_and(|name| name == "$ENV")
}

/// The `env/0` call `$ENV` lowers to — the ONE law shared by every spelling
/// of the environment binding. `$ENV` is PRE-bound, so a `def env: …;` shadows
/// the builtin NAME for ordinary calls but never this binding (`def env: 1;
/// $ENV` answers the environment object).
pub(crate) fn lower_env_call(lowerer: &mut Lowerer<'_, '_>) -> Result<ProgramNodeId, EngineCompileError> {
    let record = resolve_builtin("env", 0).ok_or_else(|| {
        EngineCompileError::Parse(ParseRejection::internal("the stdlib env/0 binding is not registered"))
    })?;
    push_node(
        &mut lowerer.nodes,
        ProgramNode::call(record.id, record.semantic_revision, Vec::new()),
    )
}

/// `$__loc__`'s value: the source location of the reference —
/// `{"file": <label>, "line": N}` with `N` counted from the program text. The
/// label is the compile source's own; the top-level program carries the
/// `<top-level>` convention (see the compile seam). A `def` reports the
/// REFERENCE site: `$__loc__` folds into a constant where the token appears —
/// never the caller's site. The module `file` label is pinned by the corpus's
/// `module` rows, not by this comment.
pub(crate) fn location_literal(
    span: Span,
    source: &SyntaxSource<'_>,
    _resources: &ResourceContext<'_>,
) -> Result<Value, EngineCompileError> {
    let text = source.text();
    let start = span.range().start.min(text.len());
    let line = 1 + text[..start].bytes().filter(|&b| b == b'\n').count();
    let mut object = jqf_data::ObjectBuilder::try_with_capacity(2)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    object
        .try_insert_last(
            jqf_data::ObjectKey::try_from_str("file")
                .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?,
            Value::try_string(source.label())
                .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?,
        )
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    object
        .try_insert_last(
            jqf_data::ObjectKey::try_from_str("line")
                .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?,
            Value::Number(Number::integer(Integer::from_i64(
                i64::try_from(line).unwrap_or(i64::MAX),
            ))),
        )
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    object
        .try_finish()
        .map(Value::Object)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))
}

/// Lowers a `$__loc__` reference in EXPRESSION position to the location
/// literal; `None` when the variable is not the location binding.
pub(crate) fn try_lower_loc_binding(
    span: Span,
    source: &SyntaxSource<'_>,
    lowerer: &mut Lowerer<'_, '_>,
) -> Result<Option<ProgramNodeId>, EngineCompileError> {
    if !is_loc_binding(span, source) {
        return Ok(None);
    }
    let value = location_literal(span, source, lowerer.resources)?;
    Ok(Some(push_node(&mut lowerer.nodes, literal_stage(value))?))
}

/// Resolves one `$x` reference against the lexical scope stack.
///
/// `$ENV` and `$__loc__` are rejected BY NAME: they are real bindings that have
/// not landed, and a generic "not defined" would misclassify them as user
/// error. Any other unbound name is the compile-time `$x is not defined`
/// (exit-3 class).
pub(crate) fn resolve_variable(
    span: Span,
    source: &SyntaxSource<'_>,
    scopes: &Scopes,
) -> Result<VarSlot, EngineCompileError> {
    let name = source
        .text()
        .get(span.range())
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("variable span out of range")))?;
    if let Some(slot) = scopes.resolve(name) {
        return Ok(slot);
    }
    match name {
        "$ENV" => Err(EngineCompileError::unsupported(
            span,
            UnsupportedConstruct::Expression("the `$ENV` environment binding"),
        )),
        "$__loc__" => Err(EngineCompileError::unsupported(
            span,
            UnsupportedConstruct::Expression("the `$__loc__` location binding"),
        )),
        _ => Err(EngineCompileError::undefined_variable(span, name)),
    }
}

/// Whether any node in the subgraph rooted at `id` contains a `.[]`
/// ([`StepAccess::Each`]) step — the count-fusion gate (see the pipe arm).
fn graph_has_each_step(nodes: &[ProgramNode], callables: &[CallableDef], id: ProgramNodeId) -> bool {
    fn walk(nodes: &[ProgramNode], callables: &[CallableDef], id: ProgramNodeId, seen: &mut [bool]) -> bool {
        let index = id.index();
        if index >= seen.len() || seen[index] {
            return false;
        }
        seen[index] = true;
        match &nodes[index] {
            ProgramNode::Stage { steps, .. } => steps.iter().any(|step| matches!(step.access(), StepAccess::Each)),
            ProgramNode::FlatMap { upstream, body }
            | ProgramNode::Binary {
                left: upstream,
                right: body,
                ..
            } => walk(nodes, callables, *upstream, seen) || walk(nodes, callables, *body, seen),
            ProgramNode::Concat { parts } => parts.iter().any(|part| walk(nodes, callables, *part, seen)),
            ProgramNode::Conditional {
                condition,
                consequent,
                alternative,
            } => {
                walk(nodes, callables, *condition, seen)
                    || walk(nodes, callables, *consequent, seen)
                    || walk(nodes, callables, *alternative, seen)
            }
            ProgramNode::Choice { left, right }
            | ProgramNode::Alternative { left, right }
            | ProgramNode::Logical { left, right, .. } => {
                walk(nodes, callables, *left, seen) || walk(nodes, callables, *right, seen)
            }
            ProgramNode::CollectArray { body } | ProgramNode::CountCollect { body } => {
                body.is_some_and(|body| walk(nodes, callables, body, seen))
            }
            ProgramNode::ConstructObject { members } => members
                .iter()
                .any(|member| walk(nodes, callables, member.key, seen) || walk(nodes, callables, member.value, seen)),
            ProgramNode::Call { args, .. } => args.iter().any(|arg| walk(nodes, callables, *arg, seen)),
            ProgramNode::CallDef {
                body,
                args,
                filter_args,
                ..
            } => {
                // Pre-fusion: `body` holds a callable-table index, not an arena node id.
                let callable_has_each = callables
                    .get(body.index())
                    .is_some_and(|callable| walk(nodes, callables, callable.body, seen));
                callable_has_each
                    || args.iter().any(|arg| walk(nodes, callables, *arg, seen))
                    || filter_args.iter().any(|arg| walk(nodes, callables, *arg, seen))
            }
            ProgramNode::EngineGenerator { init, update, extract } => {
                walk(nodes, callables, *init, seen)
                    || walk(nodes, callables, *update, seen)
                    || walk(nodes, callables, *extract, seen)
            }
            ProgramNode::EngineRng { seed } => walk(nodes, callables, *seed, seen),
            ProgramNode::Bind { source, body, .. } | ProgramNode::EngineBind { source, body, .. } => {
                walk(nodes, callables, *source, seen) || walk(nodes, callables, *body, seen)
            }
            ProgramNode::Reduce {
                source, init, update, ..
            } => {
                walk(nodes, callables, *source, seen)
                    || walk(nodes, callables, *init, seen)
                    || walk(nodes, callables, *update, seen)
            }
            ProgramNode::Foreach {
                source,
                init,
                update,
                extract,
                ..
            } => {
                walk(nodes, callables, *source, seen)
                    || walk(nodes, callables, *init, seen)
                    || walk(nodes, callables, *update, seen)
                    || extract.is_some_and(|extract| walk(nodes, callables, extract, seen))
            }
            ProgramNode::Try { body, handler } => {
                walk(nodes, callables, *body, seen) || handler.is_some_and(|h| walk(nodes, callables, h, seen))
            }
            ProgramNode::ChainBody { body } | ProgramNode::Label { body, .. } => walk(nodes, callables, *body, seen),
            ProgramNode::Modify { paths, update, .. } | ProgramNode::FactAssign { paths, update, .. } => {
                walk(nodes, callables, *paths, seen) || walk(nodes, callables, *update, seen)
            }
            ProgramNode::Empty
            | ProgramNode::CallFilter { .. }
            | ProgramNode::Break { .. }
            | ProgramNode::EnginePull { .. } => false,
            ProgramNode::Counted { source, .. } => walk(nodes, callables, *source, seen),
        }
    }
    walk(nodes, callables, id, &mut vec![false; nodes.len()])
}
