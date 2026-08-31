//! User `def`, inlining, and recursive callables.
//!
//! Push visible definitions, inline call-by-name bodies, and compile recursive
//! callables once per definition.

#[allow(clippy::wildcard_imports)]
use super::*;

/// Walks a prelude's `def a: …; def b: …; .` chain, making every definition
/// visible without lowering any of them.
///
/// Nothing is lowered here on purpose: an unused stdlib definition must cost
/// zero arena nodes, so a program that calls none of them lowers exactly as it
/// did before the prelude existed.
pub(crate) fn push_prelude_definitions<'ast>(
    mut expr: &'ast Expr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<(), EngineCompileError> {
    while let ExprKind::Definition(definition) = expr.kind() {
        let item = definition.definition.as_ref();
        let name = source
            .text()
            .get(item.name.range())
            .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("prelude name span out of range")))?;
        lowerer
            .defs
            .try_reserve(1)
            .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
        lowerer.defs.push(DefEntry {
            name: copy_string(name)?,
            arity: item.params.len(),
            params: &item.params,
            body: &item.body,
            source: *source,
            var_depth: 0,
            label_depth: 0,
            def_depth: lowerer.defs.len(),
            active: false,
            callable: None,
        });
        expr = &definition.body;
    }
    Ok(())
}

/// Lowers `def name(params): body; rest` by making the definition VISIBLE to
/// `rest` and lowering `rest`; the body itself is lowered only where it is
/// called, inlined at each call site.
///
/// Inlining rather than a call node is the whole strategy for a non-recursive
/// `def`, and it buys the two hard parts of `def` for free. Filter parameters
/// are call-by-name in the CALLER's scope: lowering each argument once,
/// caller-side, and referencing the resulting node by id at every use of the
/// parameter IS that semantics — no closure object and no environment capture
/// is needed. And because slots are allocated per lowered occurrence, two
/// inlined copies of the same body get two distinct sets of slots, preserving
/// the one-slot-per-occurrence law that a naive graph copy would violate.
///
/// Recursion is inexpressible as inlining: a call reaching a definition whose
/// body is still being lowered routes to the callable path, which compiles the
/// body once and captures filter arguments as runtime closures.
pub(crate) fn lower_definition<'ast>(
    definition: &'ast jqf_syntax::DefinitionExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let item = definition.definition.as_ref();
    let name = source
        .text()
        .get(item.name.range())
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("definition name span out of range")))?;
    lowerer
        .defs
        .try_reserve(1)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    lowerer.defs.push(DefEntry {
        name: copy_string(name)?,
        arity: item.params.len(),
        params: &item.params,
        body: &item.body,
        source: *source,
        var_depth: lowerer.scopes.entries.len(),
        label_depth: lowerer.labels.entries.len(),
        def_depth: lowerer.defs.len(),
        active: false,
        callable: None,
    });
    let lowered_body = lower_expr(&definition.body, source, lowerer);
    lowerer.defs.pop();
    lowered_body
}

/// Inlines a call to a user `def`, or returns `None` when no visible definition
/// matches `(name, arity)`.
pub(crate) fn lower_user_call<'ast>(
    call: &'ast CallExpr,
    name: &str,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Option<Result<ProgramNodeId, EngineCompileError>> {
    if call.args.is_empty() {
        return try_lower_zero_arg_user(name, call.name, source, lowerer);
    }
    if let Some(index) = lowerer
        .defs
        .iter()
        .rposition(|entry| entry.name == name && entry.arity == call.args.len())
    {
        return Some(inline_definition(index, call.name, &call.args, source, lowerer));
    }
    // A module def (an `include` or a namespaced `import`) is pre-compiled as a
    // callable; later registrations shadow earlier ones (the bind order).
    let module = lowerer
        .module_defs
        .iter()
        .rposition(|entry| entry.name == name && entry.arity == call.args.len())?;
    Some(emit_module_call(module, &call.args, source, lowerer))
}

/// Zero-arg lookup shared by a bare `name` call and the `empty` syntax form.
///
/// A filter parameter of the inlined definition, a visible `name/0` def, or a
/// module def of that arity wins; otherwise the caller keeps its own lowering
/// (`ProgramNode::Empty` for the syntax form, a builtin for an ordinary call).
pub(crate) fn try_lower_zero_arg_user<'ast>(
    name: &str,
    span: jqf_source::Span,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Option<Result<ProgramNodeId, EngineCompileError>> {
    let index = lowerer
        .defs
        .iter()
        .rposition(|entry| entry.name == name && entry.arity == 0);
    if let Some(position) = lowerer
        .params
        .iter()
        .rposition(|binding| matches!(binding, ParamBinding::Filter { name: bound, .. } if bound == name))
        && let ParamBinding::Filter { def_base, slot, .. } = &lowerer.params[position]
        && index.is_none_or(|index| index < *def_base)
    {
        if let Some(slot) = *slot {
            return Some(push_node(&mut lowerer.nodes, ProgramNode::CallFilter { slot }));
        }
        return Some(lower_filter_argument(position, lowerer));
    }
    if let Some(index) = index {
        return Some(inline_definition(index, span, &[], source, lowerer));
    }
    let module = lowerer
        .module_defs
        .iter()
        .rposition(|entry| entry.name == name && entry.arity == 0)?;
    Some(emit_module_call(module, &[], source, lowerer))
}

/// Lowers one call site of a module def: evaluate the call-site argument
/// graphs and emit a [`ProgramNode::CallDef`] to the pre-compiled body.
pub(crate) fn emit_module_call<'ast>(
    module: usize,
    args: &'ast [CallArgument],
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let callable = lowerer.module_defs[module].callable;
    let mut lowered_args = Vec::new();
    lowered_args
        .try_reserve(args.len())
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    for argument in args {
        lowered_args.push(lower_expr(&argument.expression, source, lowerer)?);
    }
    push_node(
        &mut lowerer.nodes,
        // The callable INDEX rides in the `body` slot until
        // [`crate::analysis::fuse_with_callables`] resolves it with the shared
        // body id, exactly like a recursive def's call site.
        ProgramNode::CallDef {
            body: ProgramNodeId::from_index(callable).expect("callable index stays within the arena addressing bound"),
            param_slots: Vec::new(),
            filter_slots: Vec::new(),
            args: lowered_args,
            filter_args: Vec::new(),
            tail: false,
        },
    )
}

/// Re-lowers the filter argument bound at `position`, in the lexical scope the
/// argument was WRITTEN in rather than the one it is used in.
///
/// Restoring the writer's scope is what makes the expansion hygienic in both
/// directions: the argument cannot see the callee's binders, and the callee
/// cannot see the argument's. The writer's SOURCE is restored with it, since a
/// prelude definition's call site may live in a different text entirely.
pub(crate) fn lower_filter_argument(
    position: usize,
    lowerer: &mut Lowerer<'_, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    if lowerer.nodes.len() >= MAX_LOWERED_NODES {
        let span = match &lowerer.params[position] {
            ParamBinding::Filter { expression, .. } => expression.span(),
            ParamBinding::Value { .. } => {
                return Err(EngineCompileError::Parse(ParseRejection::internal(
                    "filter parameter resolved to a value binding",
                )));
            }
        };
        return Err(EngineCompileError::unsupported(
            span,
            UnsupportedConstruct::Expression("a function expansion beyond the inlining bound"),
        ));
    }
    let ParamBinding::Filter {
        expression,
        source: writer_source,
        vars,
        labels,
        defs,
        param_depth,
        ..
    } = &lowerer.params[position]
    else {
        return Err(EngineCompileError::Parse(ParseRejection::internal(
            "filter parameter resolved to a value binding",
        )));
    };
    let (expression, writer_source, param_depth) = (*expression, *writer_source, *param_depth);
    let written_vars = clone_scope(vars)?;
    let written_labels = clone_scope(labels)?;
    let written_defs = clone_defs(defs)?;

    // Swap the WRITER's scopes in wholesale for the duration of the lowering.
    // The def stack is captured by value for the same reason the variable stack
    // is: `def f(g): g; def h: 5; f(h)` writes its argument where `h` is visible,
    // but by the time `g` is used the callee's view has already truncated it.
    let callee_vars = core::mem::replace(&mut lowerer.scopes.entries, written_vars);
    let callee_labels = core::mem::replace(&mut lowerer.labels.entries, written_labels);
    let callee_defs = core::mem::replace(&mut lowerer.defs, written_defs);
    let stashed_params = lowerer.params.split_off(param_depth.min(lowerer.params.len()));

    let lowered_argument = lower_expr(expression, &writer_source, lowerer);

    lowerer.scopes.entries = callee_vars;
    lowerer.labels.entries = callee_labels;
    lowerer.defs = callee_defs;
    lowerer.params.extend(stashed_params);
    lowered_argument
}

/// Fallibly clones the visible-definition stack.
pub(crate) fn clone_defs<'ast>(entries: &[DefEntry<'ast>]) -> Result<Vec<DefEntry<'ast>>, EngineCompileError> {
    let mut copied = Vec::new();
    copied
        .try_reserve_exact(entries.len())
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    for entry in entries {
        copied.push(DefEntry {
            name: copy_string(&entry.name)?,
            arity: entry.arity,
            params: entry.params,
            body: entry.body,
            source: entry.source,
            var_depth: entry.var_depth,
            label_depth: entry.label_depth,
            def_depth: entry.def_depth,
            active: entry.active,
            callable: entry.callable,
        });
    }
    Ok(copied)
}

/// Fallibly clones one captured scope stack.
pub(crate) fn clone_scope<S: Copy>(entries: &[(String, S)]) -> Result<Vec<(String, S)>, EngineCompileError> {
    let mut copied = Vec::new();
    copied
        .try_reserve_exact(entries.len())
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    for (name, slot) in entries {
        copied.push((copy_string(name)?, *slot));
    }
    Ok(copied)
}

/// Pairs each argument of `call` with the parameter it binds, in the CALLER's
/// lexical scope — before any of the callee's own scope has been swapped in.
///
/// A FILTER parameter is re-lowered at every use, so its binding carries the
/// argument's expression together with the scopes it was written in.
///
/// A VALUE parameter is sugar for `def f(a): a as $a | body`, so it binds
/// BOTH spellings and produces TWO bindings: the `$a` half is lowered once here
/// and its graph travels with the binding (a binding evaluates its source once),
/// and the bare `a` half is an ordinary call-by-name filter parameter. The two
/// halves are observably different whenever the argument has more than one
/// output — `def f($a): [a, $a]; f(1,2)` runs the body once per `$a` and each
/// `a` inside it re-emits both — so binding only `$a` is not the sugar.
///
/// The variable, label and definition stacks are captured BY VALUE (a depth into
/// them goes stale once the callee's body is being lowered); only the parameter
/// stack can safely be a depth, because it is never truncated beneath this frame.
pub(crate) fn capture_call_arguments<'ast>(
    index: usize,
    args: &'ast [CallArgument],
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<Vec<(ParamBinding<'ast>, Option<ProgramNodeId>)>, EngineCompileError> {
    let caller_params = lowerer.params.len();
    let mut bindings = Vec::new();
    // Two bindings per value parameter, one per filter parameter.
    bindings
        .try_reserve_exact(args.len().saturating_mul(2))
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    for (position, argument) in args.iter().enumerate() {
        let parameter = lowerer.defs[index].params[position].name;
        let def_source = lowerer.defs[index].source;
        let spelling = def_source
            .text()
            .get(parameter.range())
            .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("parameter span out of range")))?;
        let by_name = ParamBinding::Filter {
            name: copy_string(spelling.strip_prefix('$').unwrap_or(spelling))?,
            expression: &argument.expression,
            source: *source,
            vars: clone_scope(&lowerer.scopes.entries)?,
            labels: clone_scope(&lowerer.labels.entries)?,
            defs: clone_defs(&lowerer.defs)?,
            param_depth: caller_params,
            def_base: 0,
            slot: None,
        };
        if spelling.starts_with('$') {
            let graph = lower_expr(&argument.expression, source, lowerer)?;
            bindings.push((
                ParamBinding::Value {
                    name: copy_string(spelling)?,
                },
                Some(graph),
            ));
        }
        bindings.push((by_name, None));
    }
    Ok(bindings)
}

/// Lowers one call site of the definition at `index` in the visible stack.
pub(crate) fn inline_definition<'ast>(
    index: usize,
    name: Span,
    args: &'ast [CallArgument],
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    if lowerer.defs[index].active {
        // A call reaching an active definition is RECURSION. It routes to the
        // callable path, which compiles the definition's body (bounded at run
        // time by the callable-depth ceiling). The old compile-time "always
        // calls itself" refusal was a FALSE POSITIVE on programs the floor
        // completes without ever evaluating the divergent call (`first(1,
        // def f: f; f)` answers 1); divergence is now a run-time condition,
        // and the tail-round counter raises the typed depth error instead of
        // hanging.
        return lower_callable(index, args, source, lowerer);
    }
    if lowerer.nodes.len() >= MAX_LOWERED_NODES {
        return Err(EngineCompileError::unsupported(
            name,
            UnsupportedConstruct::Expression("a function expansion beyond the inlining bound"),
        ));
    }

    let bindings = capture_call_arguments(index, args, source, lowerer)?;

    // The body sees the DEFINITION's lexical scope, not the call site's: stash
    // everything bound since, restore it after.
    let (entry_vars, entry_labels, entry_defs) = {
        let entry = &lowerer.defs[index];
        (entry.var_depth, entry.label_depth, entry.def_depth)
    };
    let stashed_vars = lowerer.scopes.entries.split_off(entry_vars);
    let stashed_labels = lowerer.labels.entries.split_off(entry_labels);
    let stashed_defs = lowerer.defs.split_off(entry_defs + 1);
    let param_base = lowerer.params.len();

    // A VALUE parameter is the `arg as $a | body` sugar, so it takes a real
    // binder slot; a FILTER parameter is a graph reference and takes none.
    let mut value_binders = Vec::new();
    let mut result = (|| -> Result<ProgramNodeId, EngineCompileError> {
        for (binding, graph) in bindings {
            match binding {
                ParamBinding::Value { name } => {
                    let slot = lowerer.scopes.push(&name)?;
                    let Some(graph) = graph else {
                        return Err(EngineCompileError::Parse(ParseRejection::internal(
                            "a value parameter is missing its argument graph",
                        )));
                    };
                    value_binders.push((slot, graph));
                }
                mut filter @ ParamBinding::Filter { .. } => {
                    // The visible-definition stack is already truncated to the
                    // callee's own view, so this height is the line a `def`
                    // written INSIDE the body lands above — which is what makes
                    // such a `def` shadow this parameter.
                    if let ParamBinding::Filter { def_base, .. } = &mut filter {
                        *def_base = lowerer.defs.len();
                    }
                    lowerer
                        .params
                        .try_reserve(1)
                        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
                    lowerer.params.push(filter);
                }
            }
        }
        lowerer.defs[index].active = true;
        let body = lowerer.defs[index].body;
        let body_source = lowerer.defs[index].source;
        let lowered_body = lower_expr(body, &body_source, lowerer);
        lowerer.defs[index].active = false;
        lowered_body
    })();

    // Wrap the body in one `Bind` per value parameter, innermost last.
    if let Ok(body) = result {
        let mut wrapped = body;
        for (slot, graph) in value_binders.iter().rev() {
            wrapped = match push_node(
                &mut lowerer.nodes,
                ProgramNode::Bind {
                    source: *graph,
                    slot: *slot,
                    body: wrapped,
                    frame: false,
                },
            ) {
                Ok(id) => id,
                Err(error) => {
                    result = Err(error);
                    break;
                }
            };
        }
        if result.is_ok() {
            result = Ok(wrapped);
        }
    }

    lowerer.params.truncate(param_base);
    for _ in 0..value_binders.len() {
        lowerer.scopes.pop();
    }
    lowerer.scopes.entries.extend(stashed_vars);
    lowerer.labels.entries.extend(stashed_labels);
    lowerer.defs.extend(stashed_defs);
    result
}

/// Lowers one call site of a RECURSIVE definition at `index`.
///
/// The definition's body is compiled ONCE into the arena (its value-parameter
/// slots bound in the definition's own lexical scope, with the definition
/// active so its self-calls route back here) and every call site shares it;
/// recursion depth is bounded at run time. Filter parameters are runtime
/// closures: the call site lowers each filter argument graph into
/// `filter_args`, and a body use of the parameter is [`ProgramNode::CallFilter`]
/// against the slot bound into the shared body. Each use evaluates the captured
/// graph against the captured env, which is why a self-call `f(n-1)` does not
/// freeze `n` at the first recursive argument.
pub(crate) fn lower_callable<'ast>(
    index: usize,
    args: &'ast [CallArgument],
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let (entry_params, def_source) = {
        let entry = &lowerer.defs[index];
        (entry.params, entry.source)
    };
    let callable = if let Some(callable) = lowerer.defs[index].callable {
        callable
    } else {
        let callable = lowerer.callables.len();
        lowerer
            .callables
            .try_reserve(1)
            .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
        lowerer.callables.push(CallableDef {
            body: ProgramNodeId::from_index(0).expect("resolved after the body lowers"),
            param_slots: Vec::new(),
            filter_slots: Vec::new(),
        });
        // The cache entry is recorded BEFORE the body's own self-calls are
        // lowered: a recursive self-call re-enters `lower_callable` while
        // this body is still being lowered, and a miss would lower another
        // body — and then another — forever.
        lowerer.defs[index].callable = Some(callable);
        lowerer.callables[callable] = lower_callable_body(index, args, source, lowerer)?;
        callable
    };
    let mut value_args = Vec::new();
    let mut filter_args = Vec::new();
    value_args
        .try_reserve(args.len())
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    filter_args
        .try_reserve(args.len())
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    for (position, argument) in args.iter().enumerate() {
        let parameter = entry_params.get(position).ok_or_else(|| {
            EngineCompileError::Parse(ParseRejection::internal(
                "callable call arity exceeded its parameter list",
            ))
        })?;
        let spelling = def_source
            .text()
            .get(parameter.name.range())
            .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("parameter span out of range")))?;
        let graph = lower_expr(&argument.expression, source, lowerer)?;
        if spelling.starts_with('$') {
            value_args.push(graph);
        } else {
            filter_args.push(graph);
        }
    }
    push_node(
        &mut lowerer.nodes,
        // The callable INDEX rides in the `body` slot until
        // [`fuse_with_callables`] rewrites the node with the real body id.
        ProgramNode::CallDef {
            body: ProgramNodeId::from_index(callable).expect("callable index stays within the arena addressing bound"),
            param_slots: Vec::new(),
            filter_slots: Vec::new(),
            args: value_args,
            filter_args,
            tail: false,
        },
    )
}

/// Lowers ONE recursive definition body — value-parameter slots bound in the
/// definition's own lexical scope, filter parameters bound as
/// [`ParamBinding::Filter`] slots whose uses emit [`ProgramNode::CallFilter`]
/// — with the definition active so its self-calls route back here.
/// The caller has already registered the callable in the cache and pushed its
/// arena slot; this fills that slot in.
pub(crate) fn lower_callable_body<'ast>(
    index: usize,
    args: &'ast [CallArgument],
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<CallableDef, EngineCompileError> {
    let (entry_params, entry_body, def_source) = {
        let entry = &lowerer.defs[index];
        (entry.params, entry.body, entry.source)
    };
    let (entry_vars, entry_labels, entry_defs) = {
        let entry = &lowerer.defs[index];
        (entry.var_depth, entry.label_depth, entry.def_depth)
    };
    let stashed_vars = lowerer.scopes.entries.split_off(entry_vars);
    let stashed_labels = lowerer.labels.entries.split_off(entry_labels);
    let stashed_defs = lowerer.defs.split_off(entry_defs + 1);
    let param_base = lowerer.params.len();
    let mut param_slots = Vec::new();
    let mut filter_slots = Vec::new();
    param_slots
        .try_reserve(entry_params.len())
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    filter_slots
        .try_reserve(entry_params.len())
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    for (position, parameter) in entry_params.iter().enumerate() {
        let spelling = def_source
            .text()
            .get(parameter.name.range())
            .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("parameter span out of range")))?;
        if spelling.starts_with('$') {
            param_slots.push(lowerer.scopes.push(&copy_string(spelling)?)?);
        } else {
            // A filter parameter of a recursive callable is a runtime
            // closure slot: a body use emits CallFilter against it. The
            // call-site argument expression is stored only so the binding
            // shape matches the inlined path; uses never re-lower it.
            let argument = args.get(position).ok_or_else(|| {
                EngineCompileError::Parse(ParseRejection::internal(
                    "callable call arity exceeded its parameter list",
                ))
            })?;
            let slot = lowerer.next_filter_slot;
            lowerer.next_filter_slot = slot.checked_add(1).ok_or_else(|| {
                EngineCompileError::Parse(ParseRejection::internal(
                    "program exceeds the filter-parameter slot addressing bound",
                ))
            })?;
            lowerer
                .params
                .try_reserve(1)
                .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
            lowerer.params.push(ParamBinding::Filter {
                name: copy_string(spelling.strip_prefix('$').unwrap_or(spelling))?,
                expression: &argument.expression,
                source: *source,
                vars: clone_scope(&lowerer.scopes.entries)?,
                labels: clone_scope(&lowerer.labels.entries)?,
                defs: clone_defs(&lowerer.defs)?,
                param_depth: param_base,
                def_base: lowerer.defs.len(),
                slot: Some(slot),
            });
            filter_slots.push(slot);
        }
    }
    lowerer.defs[index].active = true;
    // The callable body runs on a NESTED evaluator (a `CallableBody` frame
    // machine). A pull of an engine binding from inside it is the carve-out,
    // rejected at lower time; the depth counts nested callables.
    lowerer.callable_depth += 1;
    let body = lower_expr(entry_body, &def_source, lowerer);
    lowerer.callable_depth -= 1;
    lowerer.defs[index].active = false;
    lowerer.params.truncate(param_base);
    for _ in 0..param_slots.len() {
        lowerer.scopes.pop();
    }
    lowerer.scopes.entries.extend(stashed_vars);
    lowerer.labels.entries.extend(stashed_labels);
    lowerer.defs.extend(stashed_defs);
    Ok(CallableDef {
        body: body?,
        param_slots,
        filter_slots,
    })
}
