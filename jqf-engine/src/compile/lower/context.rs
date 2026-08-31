//! Lowering state ([`Lowerer`]), scope stacks, and [`lower_program_unit`].

#[allow(clippy::wildcard_imports)]
use super::*;

/// The lexical scope stack lowering resolves `$x` references against.
///
/// A binder PUSHES one entry naming the variable and the fresh slot it owns,
/// lowers the sub-graph the binding scopes over, then POPS it. Resolution scans
/// from the top, so the innermost binder of a name wins (shadowing). Each push
/// takes a BRAND-NEW slot — slots are never reused across sibling scopes, because
/// a streaming emission from one binder can run downstream code while another
/// binder's frame is still live.
pub(crate) struct Scopes {
    pub(crate) entries: Vec<(String, VarSlot)>,
    pub(crate) next_slot: VarSlot,
}

impl Scopes {
    pub(crate) const fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_slot: 0,
        }
    }

    /// Pushes one binder occurrence's scope, allocating its unique slot.
    pub(crate) fn push(&mut self, name: &str) -> Result<VarSlot, EngineCompileError> {
        let slot = self.allocate_anonymous()?;
        self.open(name, slot)?;
        Ok(slot)
    }

    /// Opens a lexical extent for a slot [`Scopes::allocate_anonymous`] already
    /// handed out.
    ///
    /// A destructuring pattern needs allocation and visibility SPLIT: every one of
    /// its binders must own a slot while the frame is built, and none of them may
    /// be visible until the BODY, because a computed key expression cannot see the
    /// pattern's own variables (`. as {k:$k, ($k):$v}` is the compile-time
    /// `$k is not defined`). [`Scopes::push`] is the two steps together, which is
    /// what every single-variable binder wants.
    pub(crate) fn open(&mut self, name: &str, slot: VarSlot) -> Result<(), EngineCompileError> {
        self.entries
            .try_reserve(1)
            .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
        self.entries.push((copy_string(name)?, slot));
        Ok(())
    }

    /// Allocates a slot bound to NO name — a binder an expansion introduces that
    /// user source can never reference.
    ///
    /// `limit`'s expansion binds its count and its per-item value; naming either
    /// would let a `$n` in the user's filter capture the expansion's binder
    /// instead of the user's own. An anonymous slot cannot be resolved by
    /// [`Scopes::resolve`] at all, so the two can never collide.
    ///
    /// This is a slot ALLOCATOR with no stack extent — it pushes no `entries`
    /// row, so it must never be paired with [`Scopes::pop`]. Pairing them pops
    /// an unrelated NAMED binder: `1 as $x | [add, $x]` reported `$x is not
    /// defined`, and once the stack fell below a visible `def`'s recorded
    /// `var_depth` the `split_off` in [`inline_definition`] PANICKED
    /// (`1 as $x | def f: 2; [add, f]`). The name says allocate, not push, so
    /// that the pairing cannot look required at a call site.
    pub(crate) fn allocate_anonymous(&mut self) -> Result<VarSlot, EngineCompileError> {
        let slot = self.next_slot;
        self.next_slot = slot.checked_add(1).ok_or_else(|| {
            EngineCompileError::Parse(ParseRejection::internal(
                "program exceeds the variable slot addressing bound",
            ))
        })?;
        Ok(slot)
    }

    /// Pops the innermost binder scope, ending the variable's lexical extent.
    pub(crate) fn pop(&mut self) {
        self.entries.pop();
    }

    /// The slot the innermost enclosing binder of `name` owns, or `None` when the
    /// variable is not in lexical scope (the compile-time `$x is not defined`).
    pub(crate) fn resolve(&self, name: &str) -> Option<VarSlot> {
        self.entries
            .iter()
            .rev()
            .find(|(bound, _)| bound == name)
            .map(|(_, slot)| *slot)
    }
}

/// The lexical scope stack lowering resolves `~x` engine-binding references
/// against — the mirror of [`Scopes`], in a SEPARATE namespace.
///
/// A `~CONSTRUCTOR as ~x | BODY` binder PUSHES one entry naming the cursor and
/// the fresh machine slot it owns, lowers the body, then POPS it. Resolution
/// scans from the top, so the innermost binder of a name wins (shadowing, same
/// as `$x`). Slots are never reused across sibling scopes.
pub(crate) struct EngineScopes {
    pub(crate) entries: Vec<(String, EngineSlot)>,
    pub(crate) next_slot: EngineSlot,
}

impl EngineScopes {
    pub(crate) const fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_slot: EngineSlot(0),
        }
    }

    /// Pushes one engine-binding occurrence's scope, allocating its unique slot.
    pub(crate) fn push(&mut self, name: &str) -> Result<EngineSlot, EngineCompileError> {
        let slot = self.next_slot;
        self.next_slot = EngineSlot(slot.0.checked_add(1).ok_or_else(|| {
            EngineCompileError::Parse(ParseRejection::internal(
                "program exceeds the engine-cursor slot addressing bound",
            ))
        })?);
        self.entries
            .try_reserve(1)
            .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
        self.entries.push((copy_string(name)?, slot));
        Ok(slot)
    }

    /// Pops the innermost engine-binding scope, ending the cursor's extent.
    pub(crate) fn pop(&mut self) {
        self.entries.pop();
    }

    /// The slot the innermost enclosing binder of `name` owns, or `None` when
    /// the engine binding is not in lexical scope.
    pub(crate) fn resolve(&self, name: &str) -> Option<EngineSlot> {
        self.entries
            .iter()
            .rev()
            .find(|(bound, _)| bound == name)
            .map(|(_, slot)| *slot)
    }
}

/// The lexical scope stack lowering resolves `break $out` against.
///
/// Structurally identical to [`Scopes`] — push on entering a `label` body, pop on
/// leaving, resolve innermost-first so shadowing works (`label $o | label $o |
/// break $o` takes the INNER one) — but a SEPARATE
/// stack, because labels and variables are different namespaces: in
/// `label $x | (1 as $x | break $x)` the two `$x` are unrelated.
///
/// Allocation is in PROGRAM ORDER, which the observable marker rendering depends
/// on; see [`LabelSlot`].
pub(crate) struct LabelScopes {
    pub(crate) entries: Vec<(String, LabelSlot)>,
    pub(crate) next_slot: LabelSlot,
}

impl LabelScopes {
    pub(crate) const fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_slot: 0,
        }
    }

    /// Pushes one `label` occurrence's scope, allocating its unique slot —
    /// the allocation delegates to [`Self::allocate_anonymous`], exactly as
    /// `Scopes::push` delegates to `Scopes::allocate_anonymous`.
    pub(crate) fn push(&mut self, name: &str) -> Result<LabelSlot, EngineCompileError> {
        let slot = self.allocate_anonymous()?;
        self.entries
            .try_reserve(1)
            .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
        self.entries.push((copy_string(name)?, slot));
        Ok(slot)
    }

    /// Allocates a label slot bound to NO name — the exit target an expansion
    /// introduces, which user source can never `break` to.
    ///
    /// This is what keeps `label $out | first(break $out)` correct: the user's
    /// `break $out` resolves against the USER's stack, because the expansion's
    /// own label was never pushed under that (or any) name.
    ///
    /// Like [`Scopes::allocate_anonymous`] it has no stack extent and must never
    /// be paired with [`LabelScopes::pop`].
    pub(crate) fn allocate_anonymous(&mut self) -> Result<LabelSlot, EngineCompileError> {
        let slot = self.next_slot;
        self.next_slot = slot.checked_add(1).ok_or_else(|| {
            EngineCompileError::Parse(ParseRejection::internal(
                "program exceeds the label slot addressing bound",
            ))
        })?;
        Ok(slot)
    }

    /// Pops the innermost `label` scope, ending its lexical extent.
    pub(crate) fn pop(&mut self) {
        self.entries.pop();
    }

    /// The slot the innermost enclosing `label` of `name` owns, or `None` when
    /// the label is not in lexical scope (the `$*label-x is not defined`).
    pub(crate) fn resolve(&self, name: &str) -> Option<LabelSlot> {
        self.entries
            .iter()
            .rev()
            .find(|(bound, _)| bound == name)
            .map(|(_, slot)| *slot)
    }
}

/// How one `def` parameter is passed.
pub(crate) enum ParamBinding<'ast> {
    /// A FILTER parameter (`def f(g): …`): the caller's argument EXPRESSION plus
    /// the lexical depths it was written at.
    ///
    /// The expression is re-lowered at EVERY use, not lowered once and shared by
    /// id. Sharing looked right and is not: `def f(g): [g,g]; f(1,2)` must be
    /// `[1,2,1,2]`, and one shared subgraph produced `[1,2,null,null]` because a
    /// node is a position in the arena, not a re-entrant function. Re-lowering
    /// per use is ordinary macro expansion, and it also gives each copy its own
    /// binder slots — preserving the one-slot-per-occurrence law.
    ///
    /// The captured depths are the hygiene half: the argument lowers in the
    /// scope where it was WRITTEN, so a callee-side binder of the same spelling
    /// cannot capture it (`def f(g): 5 as $x | g; 1 as $x | f($x)` is `1`).
    ///
    /// The scopes are held BY VALUE, not as depths into the live stacks. A depth
    /// goes stale the moment the caller's scope is stashed to lower the callee's
    /// body — which is exactly when a filter argument gets used — and a stale
    /// depth silently resolves `$x` against the callee's binder instead of the
    /// caller's.
    Filter {
        name: String,
        expression: &'ast Expr,
        /// The source the argument's spans index into — the CALLER's, which may
        /// differ from the callee's when a prelude definition is being inlined.
        source: SyntaxSource<'ast>,
        vars: Vec<(String, VarSlot)>,
        labels: Vec<(String, LabelSlot)>,
        defs: Vec<DefEntry<'ast>>,
        param_depth: usize,
        /// Height of the visible-definition stack when this parameter came into
        /// scope. A `def` of the same name at or above it was written INSIDE the
        /// body and shadows the parameter, exactly as the lexical scoping says
        /// (`def f(g): def g: 9; g; f(1)` is `9`, not `1`).
        def_base: usize,
        /// Set on a recursive callable's own filter parameters: a use emits
        /// [`ProgramNode::CallFilter`] against this slot instead of re-lowering
        /// the argument. `None` on the inlined path, which still re-lowers.
        slot: Option<crate::program::FilterSlot>,
    },
    /// A VALUE parameter (`def f($a): …`), which is sugar for
    /// `def f(a): a as $a | body`. It is a name in the ordinary variable
    /// namespace, and its argument IS lowered once — a binding evaluates its
    /// source once.
    ///
    /// The sugar binds TWO names, so a value parameter also contributes a
    /// [`Self::Filter`] binding for the undecorated spelling; see
    /// [`capture_call_arguments`].
    Value { name: String },
}

/// One visible `def`, together with the lexical depths it was defined at.
///
/// The depths are what make the body lower in the DEFINITION's scope rather than
/// the call site's: `1 as $x | def f: $x; 2 as $x | f` is `1`, so the call
/// must not let the nearer binder capture the body's `$x`.
pub(crate) struct DefEntry<'ast> {
    pub(crate) name: String,
    pub(crate) arity: usize,
    pub(crate) params: &'ast [jqf_syntax::DefParameter],
    pub(crate) body: &'ast Expr,
    /// The source the body's spans index into.
    ///
    /// A definition and its call site need not come from the same text: the
    /// standard-library prelude is its own parse, and inlining one of its
    /// definitions into a user program must resolve the body's spans against
    /// the PRELUDE. `SyntaxSource` is `Copy`, so carrying it per definition
    /// costs nothing.
    pub(crate) source: SyntaxSource<'ast>,
    pub(crate) var_depth: usize,
    pub(crate) label_depth: usize,
    pub(crate) def_depth: usize,
    /// Set while this definition's own body is being lowered. A call reaching an
    /// active definition is RECURSION, which routes to the callable path.
    pub(crate) active: bool,
    /// The compiled callable body's arena slot, once the definition has been
    /// compiled for recursive calls (`None` until then). Every call site shares
    /// the one compiled body; the recursion depth is bounded at run time.
    /// Filter parameters are runtime closures bound at each call, so a
    /// filter-parameter definition uses this same single body rather than a
    /// per-argument specialization.
    pub(crate) callable: Option<usize>,
}

/// One def exposed by a loaded module: a pre-compiled callable body under an
/// exposed name (plain for `include`, `alias::name` for `import`).
pub(crate) struct ModuleDefEntry {
    pub(crate) name: String,
    pub(crate) arity: usize,
    pub(crate) callable: usize,
}

/// One library file collected before lowering so its AST outlives every call
/// site. Filter-parameter defs stay as [`DefEntry`]s (call-by-name) instead of
/// being refused as pre-compiled value-only callables.
pub(crate) struct PreparedModule {
    /// Resolved path, the lookup key for a later include/import.
    pub label: String,
    /// Directory nested includes resolve against.
    pub dir: String,
    /// Owned source text the parse tree's spans index.
    pub text: String,
    /// Owned parse tree; bound once at prepare time for the rest of the compile.
    pub syntax: jqf_syntax::ParsedSyntax<jqf_syntax::SourceUnit>,
}

impl PreparedModule {
    pub(crate) fn root(&self) -> &jqf_syntax::SourceUnit {
        self.syntax.root()
    }

    pub(crate) fn source(&self) -> SyntaxSource<'_> {
        self.syntax.verified_source(&self.label, &self.text)
    }
}

/// One prepared module bound to its retained text for the duration of lowering.
pub(crate) struct BoundModule<'module> {
    pub module: &'module PreparedModule,
}

/// Everything one loaded module's lowering produced, ready to merge into the
/// parent lowerer's arena.
pub(crate) struct ModuleLowering<'ast> {
    pub(crate) nodes: Vec<ProgramNode>,
    pub(crate) callables: Vec<CallableDef>,
    /// The defs the module exposes to the importer (plain or `alias::name`),
    /// with callable indexes into `callables`.
    pub(crate) exposed: Vec<ModuleDefEntry>,
    /// Filter-parameter defs: exported as ordinary [`DefEntry`]s so a later
    /// call site inlines them with the call-by-name law.
    pub(crate) filter_defs: Vec<DefEntry<'ast>>,
    pub(crate) slots: u32,
    pub(crate) engine_slots: u32,
    pub(crate) labels: u32,
    /// Whether the module bound the `~inputs` resident; merged into the parent
    /// so the compile result's null-first scoping covers module defs too.
    pub(crate) uses_inputs_cursor: bool,
    /// Data-import bindings (`$alias` → decoded value) collected while lowering
    /// this module, merged into the parent lowerer's `module_vars`.
    pub(crate) module_vars: Vec<(String, Value)>,
    /// Filter-parameter closure slots allocated in this module's arena (count
    /// only — merged by rebasing slot indices into the parent's numbering).
    pub(crate) filter_slots: u32,
}

/// The most arena nodes one program may lower to.
///
/// Inlining is worst-case exponential in nesting depth (`def a: .; def b: a,a;
/// def c: b,b; …`), so the expansion is bounded and a program past the bound is
/// REJECTED by name. It is never silently truncated, and never allowed to
/// exhaust memory: the executor-side frame-path fallback is what will lift
/// this, and until it exists a clear rejection is the honest answer.
///
/// [`inline_definition`] and [`lower_filter_argument`] test it themselves so their
/// refusal can name the CALL that overran. [`push_node`] tests it again, as the
/// backstop that makes the promise above true for EVERY lowering rather than only
/// the two that remembered to ask — inlining is not the only construct whose
/// expansion multiplies, and a `?//` chain nested in its own body is
/// exponential in the nesting depth for a reason the arena cannot presently
/// avoid.
///
/// The bound is on the ARENA and not on a request's resource ledger because
/// lowering happens before any request exists: a program past it is a compile-time
/// answer at exit 3, deterministic and independent of `--max-memory-bytes`, and
/// never a resource failure that surfaces mid-stream.
pub(crate) const MAX_LOWERED_NODES: usize = 200_000;

/// Lowering state: the arena under construction, the two lexical scope stacks
/// (variables and labels are separate namespaces), the visible `def` stack, and
/// the parameter bindings of the definition currently being inlined.
pub(crate) struct Lowerer<'ast, 'resources> {
    pub(crate) nodes: Vec<ProgramNode>,
    pub(crate) scopes: Scopes,
    /// The ENGINE-binding scope stack: `as ~x` binders push one entry naming
    /// the cursor and the machine slot it owns; a `~x` pull resolves against it.
    /// Structurally identical to [`Scopes`] and a SEPARATE namespace from it —
    /// `~x` and `$x` never collide.
    pub(crate) engine_scopes: EngineScopes,
    pub(crate) labels: LabelScopes,
    pub(crate) defs: Vec<DefEntry<'ast>>,
    pub(crate) callables: Vec<CallableDef>,
    /// Defs loaded from modules, in exposure order (later entries shadow).
    pub(crate) module_defs: Vec<ModuleDefEntry>,
    /// Data-import variables (`$alias` → the module's data array), in order.
    pub(crate) module_vars: Vec<(String, Value)>,
    /// `--arg`/`--argjson` bindings: names WITH the `$` prefix, later bindings
    /// shadowing earlier ones. Consulted only after every lexical scope has
    /// failed (a program binder always wins over a CLI binding) and after
    /// `module_vars` (a data import pre-binds its `$alias`). A reference
    /// matching neither the scopes nor this table stays the `$x is not
    /// defined`. Borrowed for the compile: each matching reference lowers to
    /// an owned literal copy of the bound value.
    pub(crate) cli_vars: &'resources [(String, Value)],
    pub(crate) params: Vec<ParamBinding<'ast>>,
    /// Syntax levels open on [`lower_expr`]'s own call stack.
    ///
    /// The parser bounds the tree it BUILDS, but lowering reaches trees the
    /// parser never saw — a `def` body is re-lowered at every call site, and an
    /// inlined filter argument is re-lowered inside the callee — so the walk
    /// carries its own counter against the same ceiling rather than trusting
    /// the shape of the input tree.
    pub(crate) depth: u32,
    /// The request context lowering runs under: the nesting ceiling its walk
    /// reads, the module loader and data-import decode it drives, and the
    /// ledger that lets an allocation refusal surface as a resource error.
    ///
    /// Lowering does NOT charge this account — no compile-time literal or
    /// arena charge exists; a literal's residency is carried by the compiled
    /// program itself.
    pub(crate) resources: &'resources ResourceContext<'resources>,
    /// Provably-constant lexical bindings in scope, keyed by slot: `1 as $x |
    /// .[$x:]` folds the slice bound to the literal. An entry lives exactly
    /// while its binder's BODY lowers and is popped when the scope closes, so
    /// a slot is never read as constant outside the binder that made it so.
    pub(crate) const_bindings: Vec<(VarSlot, Value)>,
    /// Recursive-CALLABLE bodies currently being lowered (nested `def`s inside
    /// a recursive definition's body). A pull of an engine binding from inside
    /// one is the carve-out: the callable body runs on a NESTED evaluator
    /// with no cursor store, so the pull is rejected at lower time with a typed
    /// error naming the restriction (the recursive-def loop idiom is written
    /// with `while`/`repeat`/`recurse` instead).
    pub(crate) callable_depth: usize,
    /// A `~generator` constructor's argument graphs currently being lowered. A
    /// pull of an engine binding inside one is CROSS-MACHINE capture: the
    /// constructor's cursor is a separately-owned machine whose graphs cannot
    /// pull a cursor the ENCLOSING machine owns, so the pull is rejected at
    /// lower time rather than failing on an empty cursor slot at run time.
    pub(crate) in_engine_constructor: usize,
    /// Next unused filter-parameter slot. Unique per recursive-callable
    /// filter-parameter occurrence, the way variable slots are unique per
    /// binder occurrence.
    pub(crate) next_filter_slot: crate::program::FilterSlot,
    /// Whether this lowering bound the `~inputs` resident: the
    /// input-sequence cursor is scoped to the null-first drive (a cursor over
    /// the input sequence collides with the per-element cursor-store reset),
    /// and the flag is how the compile result carries that scoping to the
    /// route planner.
    pub(crate) uses_inputs_cursor: bool,
    /// Whether this compile is the SPLIT lane's (`--split-exp`):
    /// the only mode that resolves an unbound `$index` reference into a
    /// runtime variable slot instead of reporting `$index is not defined`. The
    /// split expression runs once per published item with the item counter
    /// bound to that slot. Off for every ordinary compile, so a user program
    /// that references an unbound `$index` keeps the ordinary error.
    pub(crate) runtime_index: bool,
    /// The slot `$index` lowered to under the split lane, recorded once at the
    /// first reference. `None` when the expression never references it.
    pub(crate) runtime_index_slot: Option<VarSlot>,
}

/// What a lowering entry point returns: the arena, its root, the highest
/// allocated variable slot plus one, and the compiled callables.
pub(crate) type Lowered = (
    Vec<ProgramNode>,
    ProgramNodeId,
    u32,
    Vec<CallableDef>,
    bool,
    Option<VarSlot>,
);

/// Lowers one recognized program into a pipe/choice/stage arena.
///
/// Identity and static paths lower to a single [`ProgramNode::Stage`]; each pipe
/// `a | b` and group-postfix chain lowers to a [`ProgramNode::FlatMap`], and each
/// comma `a, b` to a [`ProgramNode::Choice`]. [`crate::analysis`] then fuses
/// every `FlatMap(Stage, Stage)` back into one `Stage` (path-normal form).
///
/// The third component of the result is one past the highest [`VarSlot`]
/// allocated during lowering — including anonymous expansion slots such as the
/// split lane's `$index`. The executor sizes its env vector from it once at
/// machine seed. It is zero for a binder-free program.
///
/// Production compiles go through [`try_lower_program_unit`] /
/// [`lower_program_unit`], which load modules and apply prelude/CLI bindings.
/// This prelude-free spelling is a test shortcut over one expression with no
/// module or program-unit path.
#[cfg(test)]
pub(crate) fn lower<'ast>(
    expr: &'ast Expr,
    source: &SyntaxSource<'ast>,
    resources: &ResourceContext<'_>,
) -> Result<Lowered, EngineCompileError> {
    lower_with_prelude(&[], expr, source, &[], resources, false)
}

/// Lowers one program with an optional standard-library PRELUDE in scope.
///
/// The prelude is an ordinary source text of `def`s. Its definitions are
/// pushed onto the visible stack before the user program is lowered, so a call
/// to one inlines exactly as a user `def` does — the stdlib needs no separate
/// mechanism, only the definitions themselves.
///
/// The two texts stay SEPARATE parses rather than being concatenated. That keeps
/// every user-facing byte span indexing the user's own source: a prepended
/// prelude would shift every reported span by its length, and the corpus asserts
/// on those spans.
pub(crate) fn reserve_lower_nodes(source: &SyntaxSource<'_>) -> Result<Vec<ProgramNode>, EngineCompileError> {
    let estimate = source.text().len().div_ceil(8).min(MAX_LOWERED_NODES);
    let mut nodes = Vec::new();
    nodes
        .try_reserve(estimate)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    Ok(nodes)
}

pub(crate) fn new_lowerer<'ast, 'resources>(
    nodes: Vec<ProgramNode>,
    cli_vars: &'resources [(String, Value)],
    resources: &'resources ResourceContext<'resources>,
    runtime_index: bool,
) -> Lowerer<'ast, 'resources> {
    Lowerer {
        nodes,
        scopes: Scopes::new(),
        engine_scopes: EngineScopes::new(),
        labels: LabelScopes::new(),
        defs: Vec::new(),
        callables: Vec::new(),
        module_defs: Vec::new(),
        module_vars: Vec::new(),
        cli_vars,
        params: Vec::new(),
        depth: 0,
        resources,
        const_bindings: Vec::new(),
        callable_depth: 0,
        in_engine_constructor: 0,
        next_filter_slot: 0,
        uses_inputs_cursor: false,
        runtime_index,
        runtime_index_slot: None,
    }
}

#[cfg(test)]
pub(crate) fn lower_with_prelude<'ast>(
    preludes: &[(&'ast Expr, &'ast SyntaxSource<'ast>)],
    expr: &'ast Expr,
    source: &SyntaxSource<'ast>,
    cli_vars: &[(String, Value)],
    resources: &ResourceContext<'_>,
    runtime_index: bool,
) -> Result<Lowered, EngineCompileError> {
    let mut lowerer = new_lowerer(reserve_lower_nodes(source)?, cli_vars, resources, runtime_index);
    for (prelude_root, prelude_source) in preludes {
        push_prelude_definitions(prelude_root, prelude_source, &mut lowerer)?;
    }
    let root = lower_expr(expr, source, &mut lowerer)?;
    Ok((
        lowerer.nodes,
        root,
        lowerer.scopes.next_slot,
        lowerer.callables,
        lowerer.uses_inputs_cursor,
        lowerer.runtime_index_slot,
    ))
}

/// Lowers one parsed PROGRAM unit: pushes the prelude, processes the top-level
/// items (defs, imports, includes), and lowers the final query.
///
/// Top-level `def`s become ordinary visible definitions; `import`/`include`
/// load their modules through the attached [`crate::exec::ModuleLoader`] (each
/// loaded module lowers in its own arena and is merged in), and a data import
/// pre-binds `$alias` and exposes `alias::alias`.
#[allow(
    clippy::too_many_arguments,
    reason = "the extra catalog and prelude slices are the module-lifetime inputs, not a new job"
)]
pub(crate) fn lower_program_unit<'ast>(
    preludes: &[(&'ast Expr, &'ast SyntaxSource<'ast>)],
    unit: &'ast jqf_syntax::SourceUnit,
    source: &SyntaxSource<'ast>,
    modules: &[BoundModule<'ast>],
    cli_vars: &[(String, Value)],
    resources: &ResourceContext<'_>,
    runtime_index: bool,
) -> Result<Lowered, EngineCompileError> {
    let mut lowerer = new_lowerer(reserve_lower_nodes(source)?, cli_vars, resources, runtime_index);
    for (prelude_root, prelude_source) in preludes {
        push_prelude_definitions(prelude_root, prelude_source, &mut lowerer)?;
    }
    let mut loading = BTreeSet::new();
    let mut user_defs = Vec::new();
    for item in &unit.items {
        match item {
            SourceItem::Def(item) => {
                let name = source.text().get(item.name.range()).ok_or_else(|| {
                    EngineCompileError::Parse(ParseRejection::internal("definition name span out of range"))
                })?;
                user_defs.push((copy_string(name)?, item));
            }
            SourceItem::Import(item) => {
                let exposed = process_import(item, source, &mut lowerer, None, modules, preludes, &mut loading)?;
                register_exposed_defs(&mut lowerer, exposed);
            }
            SourceItem::Include(item) => {
                let exposed = process_include(item, source, &mut lowerer, None, modules, preludes, &mut loading)?;
                register_exposed_defs(&mut lowerer, exposed);
            }
            // The module metadata declaration must be constant and an object
            // (the `Module metadata must be constant` / `… must be an object`
            // refusals).
            SourceItem::Module(item) => {
                constant_metadata(Some(&item.metadata), source)?;
            }
        }
    }
    for (name, item) in user_defs {
        push_def_entry(&name, item, source, &mut lowerer)?;
    }
    let expression = unit
        .expression
        .as_ref()
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("program unit has no query")))?;
    let root = lower_expr(expression, source, &mut lowerer)?;
    Ok((
        lowerer.nodes,
        root,
        lowerer.scopes.next_slot,
        lowerer.callables,
        lowerer.uses_inputs_cursor,
        lowerer.runtime_index_slot,
    ))
}
