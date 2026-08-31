//! `include` / `import` / module metadata lowering.
//!
//! Load library files, rebase their arenas into the parent, and expose defs.
//! Filter-parameter defs stay as [`DefEntry`]s.

#[allow(clippy::wildcard_imports)]
use super::*;

use crate::compile::parse::{bind_syntax, into_valid_syntax, parse_library_input};

/// Pushes one top-level user definition onto the visible stack.
pub(crate) fn push_def_entry<'ast>(
    name: &str,
    item: &'ast jqf_syntax::DefItem,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<(), EngineCompileError> {
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
    Ok(())
}

/// Walks `include`/`import` items (including nested ones) and owns each
/// library's parse tree so filter-parameter defs can stay as [`DefEntry`]s.
pub(crate) fn prepare_included_modules(
    unit: &jqf_syntax::SourceUnit,
    source: &SyntaxSource<'_>,
    resources: &ResourceContext<'_>,
    lib_origin: Option<&str>,
    out: &mut Vec<PreparedModule>,
    seen: &mut BTreeSet<String>,
) -> Result<(), EngineCompileError> {
    for item in &unit.items {
        match item {
            SourceItem::Include(include) => {
                prepare_one_module(
                    &include.path,
                    include.metadata.as_ref(),
                    include.span,
                    source,
                    resources,
                    lib_origin,
                    false,
                    out,
                    seen,
                )?;
            }
            SourceItem::Import(import) => {
                let alias = source.text().get(import.alias.range()).ok_or_else(|| {
                    EngineCompileError::Parse(ParseRejection::internal("import alias span out of range"))
                })?;
                if alias.starts_with('$') {
                    continue;
                }
                prepare_one_module(
                    &import.path,
                    import.metadata.as_ref(),
                    import.span,
                    source,
                    resources,
                    lib_origin,
                    false,
                    out,
                    seen,
                )?;
            }
            SourceItem::Def(_) | SourceItem::Module(_) => {}
        }
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "one resolve+parse+recurse per include/import item"
)]
pub(crate) fn prepare_one_module(
    path: &StringTemplate,
    metadata: Option<&Expr>,
    span: Span,
    source: &SyntaxSource<'_>,
    resources: &ResourceContext<'_>,
    lib_origin: Option<&str>,
    is_data: bool,
    out: &mut Vec<PreparedModule>,
    seen: &mut BTreeSet<String>,
) -> Result<(), EngineCompileError> {
    let Some(relpath) = static_template_text(path, source)? else {
        return Err(EngineCompileError::unsupported(
            path.span(),
            UnsupportedConstruct::Expression("an interpolated module path (Import path must be constant)"),
        ));
    };
    let metadata = constant_metadata(metadata, source)?;
    let search = metadata_search(metadata.as_ref());
    let Some(loader) = jqf_builtins::host::module_loader(resources) else {
        return Err(EngineCompileError::unsupported(
            span,
            UnsupportedConstruct::Expression("module resolution (no module loader attached)"),
        ));
    };
    let Some(resolved) = loader.resolve(&relpath, search.as_deref(), lib_origin, is_data) else {
        return Err(EngineCompileError::unsupported(
            span,
            UnsupportedConstruct::Expression("module not found"),
        ));
    };
    if !seen.insert(resolved.label.clone()) {
        return Ok(());
    }
    let source_ref = jqf_source::SourceRef::new(
        jqf_source::SourceId::new(100 + u32::try_from(out.len()).unwrap_or(u32::MAX)),
        jqf_source::SourceKind::Query,
    );
    let parsed = parse_library_input(source_ref, &resolved.text)?;
    let syntax = into_valid_syntax(parsed)?;
    let bound = bind_syntax(&syntax, source_ref, &resolved.label, &resolved.text)?;
    prepare_included_modules(bound.root(), bound.source(), resources, Some(&resolved.dir), out, seen)?;
    out.push(PreparedModule {
        label: resolved.label,
        dir: resolved.dir,
        text: resolved.text,
        syntax,
    });
    Ok(())
}

/// Makes a set of loaded defs visible to the rest of the compile.
pub(crate) fn register_exposed_defs(lowerer: &mut Lowerer<'_, '_>, exposed: Vec<ModuleDefEntry>) {
    for entry in exposed {
        lowerer.module_defs.push(entry);
    }
}

/// Registers one module def under a plain internal name (visible to the
/// module's own later defs, removed again when the module arena is merged).
pub(crate) fn register_module_def(
    lowerer: &mut Lowerer<'_, '_>,
    name: &str,
    arity: usize,
    callable: usize,
) -> Result<(), EngineCompileError> {
    lowerer.module_defs.push(ModuleDefEntry {
        name: copy_string(name)?,
        arity,
        callable,
    });
    Ok(())
}

/// Processes one `import` item: resolves the module (or data file) and returns
/// the defs it exposes to the importing scope.
pub(crate) fn process_import<'ast>(
    item: &'ast ImportItem,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
    lib_origin: Option<&str>,
    modules: &[BoundModule<'ast>],
    preludes: &[(&'ast Expr, &'ast SyntaxSource<'ast>)],
    loading: &mut BTreeSet<String>,
) -> Result<Vec<ModuleDefEntry>, EngineCompileError> {
    let Some(relpath) = static_template_text(&item.path, source)? else {
        return Err(EngineCompileError::unsupported(
            item.path.span(),
            UnsupportedConstruct::Expression("an interpolated module path (Import path must be constant)"),
        ));
    };
    let metadata = constant_metadata(item.metadata.as_ref(), source)?;
    let search = metadata_search(metadata.as_ref());
    let alias_text = source
        .text()
        .get(item.alias.range())
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("import alias span out of range")))?;
    let is_data = alias_text.starts_with('$');
    let loader = jqf_builtins::host::module_loader(lowerer.resources).ok_or_else(|| {
        EngineCompileError::unsupported(
            item.span,
            UnsupportedConstruct::Expression("module resolution (no module loader attached)"),
        )
    })?;
    let Some(resolved) = loader.resolve(&relpath, search.as_deref(), lib_origin, is_data) else {
        return Err(EngineCompileError::unsupported(
            item.span,
            UnsupportedConstruct::Expression("module not found"),
        ));
    };
    if is_data {
        let alias = alias_text.strip_prefix('$').unwrap_or(alias_text);
        let data = jqf_builtins::semantics::decode::json(&resolved.text, lowerer.resources).map_err(|_| {
            EngineCompileError::unsupported(
                item.span,
                UnsupportedConstruct::Expression("an invalid data module payload"),
            )
        })?;
        let mut array = Array::try_new().map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
        array
            .try_push(data)
            .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
        let value = Value::Array(array);
        lowerer.module_vars.push((copy_string(alias_text)?, value.clone()));
        // The `$d::d` spelling is a VARIABLE reference in the syntax (not a
        // qualified call), so the data array is pre-bound under both spellings.
        lowerer
            .module_vars
            .push((copy_string(&alloc::format!("{alias_text}::{alias}"))?, value));
        return Ok(Vec::new());
    }
    if loading.contains(&resolved.label) {
        return Err(EngineCompileError::circular_import(item.span, &resolved.label));
    }
    let module = lower_bound_module(
        &resolved,
        Some(alias_text),
        lowerer.cli_vars,
        lowerer.resources,
        modules,
        preludes,
        loading,
    )?;
    merge_module(lowerer, module)
}

/// Processes one `include` item: resolves the module and returns its defs under
/// their PLAIN names.
pub(crate) fn process_include<'ast>(
    item: &'ast IncludeItem,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
    lib_origin: Option<&str>,
    modules: &[BoundModule<'ast>],
    preludes: &[(&'ast Expr, &'ast SyntaxSource<'ast>)],
    loading: &mut BTreeSet<String>,
) -> Result<Vec<ModuleDefEntry>, EngineCompileError> {
    let Some(relpath) = static_template_text(&item.path, source)? else {
        return Err(EngineCompileError::unsupported(
            item.path.span(),
            UnsupportedConstruct::Expression("an interpolated module path (Import path must be constant)"),
        ));
    };
    let metadata = constant_metadata(item.metadata.as_ref(), source)?;
    let search = metadata_search(metadata.as_ref());
    let loader = jqf_builtins::host::module_loader(lowerer.resources).ok_or_else(|| {
        EngineCompileError::unsupported(
            item.span,
            UnsupportedConstruct::Expression("module resolution (no module loader attached)"),
        )
    })?;
    // The authored `{search: …}` list REPLACES the loader's defaults: `include
    // "m" {search: "./custom"}` with `-L ./default` resolves ./custom/m.jq,
    // not ./default/m.jq.
    let Some(resolved) = loader.resolve(&relpath, search.as_deref(), lib_origin, false) else {
        return Err(EngineCompileError::unsupported(
            item.span,
            UnsupportedConstruct::Expression("module not found"),
        ));
    };
    if loading.contains(&resolved.label) {
        return Err(EngineCompileError::circular_import(item.span, &resolved.label));
    }
    let module = lower_bound_module(
        &resolved,
        None,
        lowerer.cli_vars,
        lowerer.resources,
        modules,
        preludes,
        loading,
    )?;
    merge_module(lowerer, module)
}

/// Looks up a prepared module by resolved label and lowers it.
pub(crate) fn lower_bound_module<'ast>(
    loaded: &crate::exec::LoadedModule,
    prefix: Option<&str>,
    cli_vars: &[(String, Value)],
    resources: &ResourceContext<'_>,
    modules: &[BoundModule<'ast>],
    preludes: &[(&'ast Expr, &'ast SyntaxSource<'ast>)],
    loading: &mut BTreeSet<String>,
) -> Result<ModuleLowering<'ast>, EngineCompileError> {
    let prepared = modules.iter().find(|entry| entry.module.label == loaded.label);
    let Some(prepared) = prepared else {
        return Err(EngineCompileError::Parse(ParseRejection::internal(
            "prepared module catalog missed a resolved library",
        )));
    };
    lower_module(
        &prepared.module.label,
        prepared.module.root(),
        &prepared.module.source(),
        &prepared.module.dir,
        prefix,
        cli_vars,
        resources,
        modules,
        preludes,
        loading,
    )
}

/// Lowers one loaded module in its OWN lowerer, returning the arena, callables,
/// and the defs it exposes.
///
/// Value-parameter defs compile once as callables.
/// Filter-parameter defs stay as [`DefEntry`]s over the prepared AST so a
/// later call site inlines them with the call-by-name law. The parent merges
/// the arena with [`merge_module`].
#[allow(
    clippy::too_many_lines,
    reason = "one lowering per module item family: the module merge is read as a single table"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "one lowering per module item family: the module merge is read as a single table"
)]
pub(crate) fn lower_module<'ast>(
    label: &str,
    unit: &'ast jqf_syntax::SourceUnit,
    module_source: &SyntaxSource<'ast>,
    dir: &str,
    prefix: Option<&str>,
    cli_vars: &[(String, Value)],
    resources: &ResourceContext<'_>,
    modules: &[BoundModule<'ast>],
    preludes: &[(&'ast Expr, &'ast SyntaxSource<'ast>)],
    loading: &mut BTreeSet<String>,
) -> Result<ModuleLowering<'ast>, EngineCompileError> {
    if !loading.insert(label.to_owned()) {
        let span = unit
            .expression
            .as_ref()
            .map(jqf_syntax::Expr::span)
            .or_else(|| {
                unit.items.first().map(|item| match item {
                    jqf_syntax::SourceItem::Module(module) => module.span,
                    jqf_syntax::SourceItem::Import(import) => import.span,
                    jqf_syntax::SourceItem::Include(include) => include.span,
                    jqf_syntax::SourceItem::Def(def) => def.span,
                })
            })
            .unwrap_or_else(|| Span::try_new(0, 0).expect("empty span is valid"));
        return Err(EngineCompileError::circular_import(span, label));
    }
    let lowered = lower_module_body(
        label,
        unit,
        module_source,
        dir,
        prefix,
        cli_vars,
        resources,
        modules,
        preludes,
        loading,
    );
    loading.remove(label);
    lowered
}

#[allow(
    clippy::too_many_lines,
    reason = "one lowering per module item family: the module merge is read as a single table"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "one lowering per module item family: the module merge is read as a single table"
)]
fn lower_module_body<'ast>(
    _label: &str,
    unit: &'ast jqf_syntax::SourceUnit,
    module_source: &SyntaxSource<'ast>,
    dir: &str,
    prefix: Option<&str>,
    cli_vars: &[(String, Value)],
    resources: &ResourceContext<'_>,
    modules: &[BoundModule<'ast>],
    preludes: &[(&'ast Expr, &'ast SyntaxSource<'ast>)],
    loading: &mut BTreeSet<String>,
) -> Result<ModuleLowering<'ast>, EngineCompileError> {
    let mut lowerer = new_lowerer(Vec::new(), cli_vars, resources, false);
    // Modules see the same prelude the parent compiled: the trees live for
    // the whole compile, so a filter-parameter def exported from this module
    // can share that lifetime.
    for (prelude_root, prelude_source) in preludes {
        push_prelude_definitions(prelude_root, prelude_source, &mut lowerer)?;
    }
    let mut own: Vec<ModuleDefEntry> = Vec::new();
    let mut filter_defs: Vec<DefEntry<'ast>> = Vec::new();
    for item in &unit.items {
        match item {
            SourceItem::Import(import) => {
                let exposed = process_import(
                    import,
                    module_source,
                    &mut lowerer,
                    Some(dir),
                    modules,
                    preludes,
                    loading,
                )?;
                register_exposed_defs(&mut lowerer, exposed);
            }
            SourceItem::Include(include) => {
                let exposed = process_include(
                    include,
                    module_source,
                    &mut lowerer,
                    Some(dir),
                    modules,
                    preludes,
                    loading,
                )?;
                register_exposed_defs(&mut lowerer, exposed);
            }
            SourceItem::Module(item) => {
                constant_metadata(Some(&item.metadata), module_source)?;
            }
            SourceItem::Def(def) => {
                let name = module_source.text().get(def.name.range()).ok_or_else(|| {
                    EngineCompileError::Parse(ParseRejection::internal("module definition name span out of range"))
                })?;
                let arity = def.params.len();
                if def_has_filter_parameter(&def.params, module_source) {
                    push_def_entry(name, def, module_source, &mut lowerer)?;
                    let Some(last) = lowerer.defs.last() else {
                        return Err(EngineCompileError::Parse(ParseRejection::internal(
                            "filter-parameter module def was not pushed",
                        )));
                    };
                    let mut exported = clone_defs(core::slice::from_ref(last))?;
                    let Some(mut entry) = exported.pop() else {
                        return Err(EngineCompileError::Parse(ParseRejection::internal(
                            "filter-parameter module def clone was empty",
                        )));
                    };
                    if let Some(prefix) = prefix {
                        entry.name = alloc::format!("{prefix}::{}", entry.name);
                    }
                    filter_defs.push(entry);
                } else {
                    let callable = compile_module_callable(&def.params, &def.body, module_source, &mut lowerer)?;
                    register_module_def(&mut lowerer, name, arity, callable)?;
                    own.push(ModuleDefEntry {
                        name: copy_string(name)?,
                        arity,
                        callable,
                    });
                }
            }
        }
    }
    let exposed = match prefix {
        Some(prefix) => own
            .into_iter()
            .map(|mut entry| {
                entry.name = alloc::format!("{prefix}::{}", entry.name);
                entry
            })
            .collect(),
        None => own,
    };
    Ok(ModuleLowering {
        nodes: lowerer.nodes,
        callables: lowerer.callables,
        exposed,
        filter_defs,
        slots: lowerer.scopes.next_slot,
        engine_slots: lowerer.engine_scopes.next_slot.0,
        labels: lowerer.labels.next_slot,
        uses_inputs_cursor: lowerer.uses_inputs_cursor,
        module_vars: lowerer.module_vars,
        filter_slots: lowerer.next_filter_slot,
    })
}

/// Whether any parameter is a call-by-name filter (an undecorated identifier).
pub(crate) fn def_has_filter_parameter(params: &[DefParameter], source: &SyntaxSource<'_>) -> bool {
    params.iter().any(|parameter| {
        source
            .text()
            .get(parameter.name.range())
            .is_some_and(|spelling| !spelling.starts_with('$'))
    })
}

/// Merges one module's arena into the parent: appends its nodes and callables
/// with every node id, binder slot, and label slot rebased into the parent's
/// numbering, and registers the exposed defs.
pub(crate) fn merge_module<'ast>(
    lowerer: &mut Lowerer<'ast, '_>,
    module: ModuleLowering<'ast>,
) -> Result<Vec<ModuleDefEntry>, EngineCompileError> {
    let node_base = lowerer.nodes.len();
    let callable_base = lowerer.callables.len();
    let slot_base = lowerer.scopes.next_slot;
    let engine_slot_base = lowerer.engine_scopes.next_slot.0;
    let label_base = lowerer.labels.next_slot;
    let filter_slot_base = lowerer.next_filter_slot;
    // A module def body may bind `~inputs`; the resident's null-first scoping
    // travels with the module into the merged program.
    lowerer.uses_inputs_cursor |= module.uses_inputs_cursor;
    for mut node in module.nodes {
        rebase_node(
            &mut node,
            node_base,
            callable_base,
            slot_base,
            engine_slot_base,
            label_base,
            filter_slot_base,
        );
        push_node(&mut lowerer.nodes, node)?;
    }
    for mut callable in module.callables {
        callable.body = ProgramNodeId::from_index(callable.body.index() + node_base)
            .expect("module arena stays within the addressing bound");
        for slot in &mut callable.param_slots {
            *slot += slot_base;
        }
        for slot in &mut callable.filter_slots {
            *slot += filter_slot_base;
        }
        lowerer.callables.push(callable);
    }
    let mut adjusted = Vec::new();
    adjusted
        .try_reserve_exact(module.exposed.len())
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    for mut entry in module.exposed {
        entry.callable += callable_base;
        adjusted.push(entry);
    }
    lowerer.module_vars.extend(module.module_vars);
    // Filter-parameter defs join the parent's visible `def` stack so a later
    // call site inlines them. They do not capture the includer's binders
    // (var/label depth 0); they see only defs already visible, plus themselves.
    for mut entry in module.filter_defs {
        entry.var_depth = 0;
        entry.label_depth = 0;
        entry.def_depth = lowerer.defs.len();
        lowerer
            .defs
            .try_reserve(1)
            .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
        lowerer.defs.push(entry);
    }
    lowerer.scopes.next_slot = lowerer
        .scopes
        .next_slot
        .checked_add(module.slots)
        .ok_or(EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    lowerer.engine_scopes.next_slot = EngineSlot(
        lowerer
            .engine_scopes
            .next_slot
            .0
            .checked_add(module.engine_slots)
            .ok_or(EngineCompileError::Resource(ResourceError::AllocationFailed))?,
    );
    lowerer.labels.next_slot = lowerer
        .labels
        .next_slot
        .checked_add(module.labels)
        .ok_or(EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    lowerer.next_filter_slot = lowerer
        .next_filter_slot
        .checked_add(module.filter_slots)
        .ok_or(EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    Ok(adjusted)
}

/// Rebases every arena edge, binder slot, and label slot in one node from a
/// module arena into the parent's numbering.
#[allow(
    clippy::too_many_lines,
    reason = "one arm per arena node family: the rebase walk is read as a single table"
)]
pub(crate) fn rebase_node(
    node: &mut ProgramNode,
    node_base: usize,
    callable_base: usize,
    slot_base: u32,
    engine_slot_base: u32,
    label_base: u32,
    filter_slot_base: u32,
) {
    let rebase_id = |id: &mut ProgramNodeId| {
        *id =
            ProgramNodeId::from_index(id.index() + node_base).expect("module arena stays within the addressing bound");
    };
    match node {
        ProgramNode::Stage { start, steps } => {
            if let StageStart::Variable(slot) = start {
                *slot += slot_base;
            }
            for step in steps {
                match step.access_mut() {
                    StepAccess::DynVar(slot) | StepAccess::DynNodeAccessor(slot) | StepAccess::DynAttribute(slot) => {
                        *slot += slot_base;
                    }
                    StepAccess::Slice(bounds) => {
                        let bounds = bounds.as_mut();
                        if let SliceBound::Var(slot) = &mut bounds.start {
                            *slot += slot_base;
                        }
                        if let SliceBound::Var(slot) = &mut bounds.end {
                            *slot += slot_base;
                        }
                    }
                    _ => {}
                }
            }
        }
        ProgramNode::FlatMap { upstream, body } => {
            rebase_id(upstream);
            rebase_id(body);
        }
        // `Choice`, `Binary`, `Alternative` and `Logical` all rebase the same
        // two children; the arms are merged because their bodies are identical.
        ProgramNode::Choice { left, right }
        | ProgramNode::Binary { left, right, .. }
        | ProgramNode::Alternative { left, right }
        | ProgramNode::Logical { left, right, .. } => {
            rebase_id(left);
            rebase_id(right);
        }
        ProgramNode::Concat { parts } => {
            for part in parts {
                rebase_id(part);
            }
        }
        ProgramNode::CollectArray { body } | ProgramNode::CountCollect { body } => {
            if let Some(body) = body {
                rebase_id(body);
            }
        }
        ProgramNode::ConstructObject { members } => {
            for member in members {
                rebase_id(&mut member.key);
                rebase_id(&mut member.value);
            }
        }
        ProgramNode::Call { args, .. } => {
            for arg in args {
                rebase_id(arg);
            }
        }
        ProgramNode::CallDef {
            body,
            param_slots,
            filter_slots,
            args,
            filter_args,
            ..
        } => {
            *body = ProgramNodeId::from_index(body.index() + callable_base)
                .expect("module arena stays within the addressing bound");
            for slot in param_slots {
                *slot += slot_base;
            }
            for slot in filter_slots {
                *slot += filter_slot_base;
            }
            for arg in args {
                rebase_id(arg);
            }
            for arg in filter_args {
                rebase_id(arg);
            }
        }
        ProgramNode::Conditional {
            condition,
            consequent,
            alternative,
        } => {
            rebase_id(condition);
            rebase_id(consequent);
            rebase_id(alternative);
        }

        ProgramNode::Try { body, handler } => {
            rebase_id(body);
            if let Some(handler) = handler {
                rebase_id(handler);
            }
        }
        ProgramNode::ChainBody { body } => rebase_id(body),
        ProgramNode::Empty => {}
        ProgramNode::CallFilter { slot } => {
            *slot += filter_slot_base;
        }
        ProgramNode::Bind { source, slot, body, .. } => {
            rebase_id(source);
            *slot += slot_base;
            rebase_id(body);
        }
        ProgramNode::EngineBind { source, slot, body } => {
            rebase_id(source);
            *slot = EngineSlot(slot.0 + engine_slot_base);
            rebase_id(body);
        }
        ProgramNode::EnginePull { slot, .. } => {
            *slot = EngineSlot(slot.0 + engine_slot_base);
        }
        ProgramNode::EngineGenerator { init, update, extract } => {
            rebase_id(init);
            rebase_id(update);
            rebase_id(extract);
        }
        ProgramNode::EngineRng { seed } => rebase_id(seed),
        ProgramNode::Reduce {
            source,
            slot,
            init,
            update,
            keyed_collect: _,
        } => {
            rebase_id(source);
            *slot += slot_base;
            rebase_id(init);
            rebase_id(update);
        }
        ProgramNode::Foreach {
            source,
            slot,
            init,
            update,
            extract,
        } => {
            rebase_id(source);
            *slot += slot_base;
            rebase_id(init);
            rebase_id(update);
            if let Some(extract) = extract {
                rebase_id(extract);
            }
        }
        ProgramNode::Counted {
            source, count, stop, ..
        } => {
            rebase_id(source);
            *count += slot_base;
            *stop += label_base;
        }
        ProgramNode::Label { slot, body } => {
            *slot += label_base;
            rebase_id(body);
        }
        ProgramNode::Break { slot } => {
            *slot += label_base;
        }
        // A `FactAssign` rebases the same two children (the role is not a slot).
        ProgramNode::Modify { paths, update, .. } => {
            rebase_id(paths);
            rebase_id(update);
        }
        ProgramNode::FactAssign {
            paths,
            update,
            selector,
            ..
        } => {
            rebase_id(paths);
            rebase_id(update);
            if let Some((slot, _)) = selector {
                *slot += slot_base;
            }
        }
    }
}

/// Compiles one module def's body ONCE as a callable. Value parameters bind
/// ordinary slots. Filter-parameter defs never enter here: they stay as
/// [`DefEntry`]s over the prepared module AST so a later call site inlines
/// them with the call-by-name law.
pub(crate) fn compile_module_callable<'ast>(
    params: &'ast [DefParameter],
    body: &'ast Expr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<usize, EngineCompileError> {
    // The spellings are fetched ONCE and validated before the callable is
    // created.
    let spellings = params
        .iter()
        .map(|parameter| {
            source.text().get(parameter.name.range()).ok_or_else(|| {
                EngineCompileError::Parse(ParseRejection::internal("module parameter span out of range"))
            })
        })
        .collect::<Result<Vec<&'ast str>, _>>()?;
    for (index, spelling) in spellings.iter().enumerate() {
        if !spelling.starts_with('$') {
            return Err(EngineCompileError::unsupported(
                params[index].name,
                UnsupportedConstruct::Expression(
                    "a module function with a filter parameter (module defs bind value parameters)",
                ),
            ));
        }
    }
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
    let mut param_slots = Vec::new();
    for spelling in &spellings {
        param_slots.push(lowerer.scopes.push(&copy_string(spelling)?)?);
    }
    let body_id = lower_expr(body, source, lowerer)?;
    for _ in params {
        lowerer.scopes.pop();
    }
    lowerer.callables[callable] = CallableDef {
        body: body_id,
        param_slots,
        filter_slots: Vec::new(),
    };
    Ok(callable)
}

/// Evaluates the module/import/include metadata expression as a CONSTANT and
/// requires an object (the `Module metadata must be constant` / `… must be an
/// object` refusals).
pub(crate) fn constant_metadata<'ast>(
    expr: Option<&'ast Expr>,
    source: &SyntaxSource<'ast>,
) -> Result<Option<Value>, EngineCompileError> {
    let Some(expr) = expr else {
        return Ok(None);
    };
    let value = evaluate_constant(expr, source)?;
    if !matches!(value.untagged(), Value::Object(_)) {
        return Err(EngineCompileError::unsupported(
            expr.span(),
            UnsupportedConstruct::Expression("module metadata must be an object (Module metadata must be an object)"),
        ));
    }
    Ok(Some(value))
}

/// The authored `search` metadata as one entry per string (`None` when absent).
///
/// The contract is STRINGS-ONLY, and it stays explicit: a non-string entry in
/// a `search` array is SKIPPED, never coerced and never fatal. The field is a
/// resolution HINT for the module loader; refusing a module over malformed
/// hint metadata would misreport a program error as a missing module.
pub(crate) fn metadata_search(metadata: Option<&Value>) -> Option<Vec<String>> {
    let Value::Object(object) = metadata?.untagged() else {
        return None;
    };
    let search = object.get("search")?;
    match search.untagged() {
        Value::String(text) => Some(vec![String::from(text.as_str())]),
        Value::Array(array) => Some(
            array
                .iter()
                .filter_map(|entry| match entry.untagged() {
                    Value::String(text) => Some(String::from(text.as_str())),
                    _ => None,
                })
                .collect(),
        ),
        _ => None,
    }
}

/// Best-effort constant fold of an array constructor for the MAIN lowering
/// path: `[]` and `[1,2]` lower to one [`StageStart::Literal`] producer instead
/// of a per-element `CollectArray` construction. The comma inside the body is
/// a sequence the constant walker folds — `[1,2]` concatenates in evaluation
/// order. Returns `None` for any non-constant shape. A `Parse` or `Resource`
/// failure from the walker propagates; `Unsupported` declines the fold and
/// falls back to the ordinary per-element path.
///
/// The constant-result folding — whole constant containers lower to one
/// literal producer — is the mechanism; this is the same idea lowered through
/// the literal producer.
pub(crate) fn try_fold_constant_array<'ast>(
    expression: Option<&'ast Expr>,
    source: &SyntaxSource<'ast>,
) -> Result<Option<Value>, EngineCompileError> {
    let mut values = Vec::new();
    if let Some(generator) = expression
        && !jqf_builtins::constant::fold_constant_seq(generator, source, &mut values)?
    {
        return Ok(None);
    }
    let mut array = Array::try_new().map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    for value in values {
        array
            .try_push(value)
            .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    }
    Ok(Some(Value::Array(array)))
}

/// Best-effort constant fold of an object constructor for the MAIN lowering
/// path: `{a:1, b:2}` lowers to one literal producer instead of per-element
/// `ConstructObject` construction. The object builds through the SAME
/// `ObjectBuilder` law as runtime construction, so the first-duplicate-fixes-
/// position / final-occurrence-supplies-the-value law is inherited verbatim.
/// Returns `None` for any member that is not provably constant (dynamic or
/// interpolated keys, shorthand members, non-constant values).
pub(crate) fn try_fold_constant_object<'ast>(
    members: &'ast [ObjectMember],
    source: &SyntaxSource<'ast>,
) -> Result<Option<Value>, EngineCompileError> {
    let mut builder = jqf_data::ObjectBuilder::try_with_capacity(members.len())
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    for member in members {
        // A dynamic/interpolated key, or any key the constant path cannot
        // name: decline the fold, the ordinary member lowering owns it.
        let Ok(key) = constant_object_key(&member.key, member.span, source) else {
            return Ok(None);
        };
        let Some(value) = member.value.as_ref() else {
            // A shorthand member reads the input; never constant.
            return Ok(None);
        };
        let Ok(value) = evaluate_constant(value, source) else {
            return Ok(None);
        };
        builder
            .try_insert_last(key, value)
            .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    }
    builder
        .try_finish()
        .map(Value::Object)
        .map(Some)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))
}
