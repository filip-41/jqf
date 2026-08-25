//! AST-to-arena lowering for the static-path/literal/constructor subset.
//!
//! [`lower`] recognizes identity `.`, static forward field/index paths and
//! `.[]` iteration (each with a per-component optional `?`), the pipe `a | b`
//! composition of those, the comma `a, b` choice, parenthesized groups `(expr)`,
//! scalar literals (`null`, `true`, `false`, numbers, plain strings — NO
//! interpolation), and the `[body?]` / `{members}` constructors, all with
//! ordinary postfix composition — over the shared syntax frontend, emitting a
//! [`crate::program`] arena. A scalar literal lowers to a [`StageStart::Literal`]
//! stage; identity/static paths to a [`StageStart::Current`] [`ProgramNode::Stage`];
//! pipe/group-postfix to [`ProgramNode::FlatMap`]; comma to [`ProgramNode::Choice`];
//! `[…]` to [`ProgramNode::CollectArray`]; and `{…}` to
//! [`ProgramNode::ConstructObject`] (member commas are contextual separators, never
//! `Choice`; static keys are singleton literal-string producers). Groups produce
//! no node of their own, and a postfix chain on any base composes (a `Stage` base
//! fuses the steps on, any other base becomes `FlatMap(base, Stage[steps])`). The
//! control-flow operators lower here too: `if`/`elif`/`else` to nested
//! [`ProgramNode::Conditional`] (missing `else` synthesizing an identity arm),
//! `and`/`or` to [`ProgramNode::Logical`], and `//` to [`ProgramNode::Alternative`].
//! The variable-binding family lowers here too: `empty` to [`ProgramNode::Empty`],
//! `SOURCE as $x | BODY` to [`ProgramNode::Bind`], and `reduce`/`foreach` to
//! [`ProgramNode::Reduce`]/[`ProgramNode::Foreach`]. Lowering owns VARIABLE
//! RESOLUTION: a lexical scope stack maps each `$x` reference to the dense slot of
//! the innermost enclosing binder OCCURRENCE (one fresh slot per occurrence, never
//! reused), so a `$x` term becomes a [`StageStart::Variable`] stage and `.[$i]` a
//! [`StepAccess::DynVar`] step; an unbound name is an undefined-variable compile
//! error.
//!
//! It owns the complete `UnsupportedConstruct` rejection surface: every
//! construct outside the subset is rejected by name and span. The READ
//! accessor surface (direct, quoted, and dynamic `.@`/`.&` selectors) lowers
//! to real accessor steps; accessor WRITE stays rejected. Standalone
//! term-level `?` error suppression (`.?`, the second `?` of `.a??`, and
//! a `?` on a group term such as `(.a)?`) is rejected by name as the `try`-sugar it
//! desugars from — distinct from the per-component optional flag.
//!
//! It does not fuse the graph, derive analysis facts, charge the ledger, or
//! lower into a codec requirement — it produces the arena and rejects everything
//! else.

use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use jqf_data::{Array, Integer, Number, Value};
use jqf_resource::{ResourceContext, ResourceError};
use jqf_source::Span;
use jqf_syntax::{
    AccessorSelector, AssignmentExpr, AssignmentOp, BinaryOp, CallArgument, CallExpr, ConditionalExpr, DefParameter,
    Expr, ExprKind, FieldSelector, ImportItem, IncludeItem, LoopExpr, ObjectKey, ObjectMember, Pattern, PatternKind,
    PostfixExpr, PostfixSegment, PostfixStep, SourceItem, StringTemplate, SyntaxSource, TemplateSegment, UnaryOp,
};

use super::{EngineCompileError, ParseRejection, UnsupportedConstruct, try_copy_str};
use crate::program::{
    BinaryKind, CallableDef, CountedKind, EnginePullKind, EngineSlot, LabelSlot, LogicalOp, ModifyMode,
    ObjectMemberNode, ProgramNode, ProgramNodeId, SliceBound, SliceBounds, StageStart, StageStep, StepAccess, VarSlot,
};
use jqf_builtins::constant::{
    constant_object_key, decode_literal_segment, evaluate_constant, lower_number, static_template_text,
};
use jqf_builtins::registry::{BuiltinDispatch, BuiltinExecution, Lowering, dispatch, resolve_builtin};
use jqf_builtins::semantics::arith::{ArithOp, compute_number};

/// The lexical scope stack lowering resolves `$x` references against.
///
/// A binder PUSHES one entry naming the variable and the fresh slot it owns,
/// lowers the sub-graph the binding scopes over, then POPS it. Resolution scans
/// from the top, so the innermost binder of a name wins (shadowing). Each push
/// takes a BRAND-NEW slot — slots are never reused across sibling scopes, because
/// a streaming emission from one binder can run downstream code while another
/// binder's frame is still live.
struct Scopes {
    entries: Vec<(String, VarSlot)>,
    next_slot: VarSlot,
}

impl Scopes {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_slot: 0,
        }
    }

    /// Pushes one binder occurrence's scope, allocating its unique slot.
    fn push(&mut self, name: &str) -> Result<VarSlot, EngineCompileError> {
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
    fn open(&mut self, name: &str, slot: VarSlot) -> Result<(), EngineCompileError> {
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
    fn allocate_anonymous(&mut self) -> Result<VarSlot, EngineCompileError> {
        let slot = self.next_slot;
        self.next_slot = slot.checked_add(1).ok_or_else(|| {
            EngineCompileError::Parse(ParseRejection::internal(
                "program exceeds the variable slot addressing bound",
            ))
        })?;
        Ok(slot)
    }

    /// Pops the innermost binder scope, ending the variable's lexical extent.
    fn pop(&mut self) {
        self.entries.pop();
    }

    /// The slot the innermost enclosing binder of `name` owns, or `None` when the
    /// variable is not in lexical scope (the compile-time `$x is not defined`).
    fn resolve(&self, name: &str) -> Option<VarSlot> {
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
struct EngineScopes {
    entries: Vec<(String, EngineSlot)>,
    next_slot: EngineSlot,
}

impl EngineScopes {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_slot: EngineSlot(0),
        }
    }

    /// Pushes one engine-binding occurrence's scope, allocating its unique slot.
    fn push(&mut self, name: &str) -> Result<EngineSlot, EngineCompileError> {
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
    fn pop(&mut self) {
        self.entries.pop();
    }

    /// The slot the innermost enclosing binder of `name` owns, or `None` when
    /// the engine binding is not in lexical scope.
    fn resolve(&self, name: &str) -> Option<EngineSlot> {
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
struct LabelScopes {
    entries: Vec<(String, LabelSlot)>,
    next_slot: LabelSlot,
}

impl LabelScopes {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_slot: 0,
        }
    }

    /// Pushes one `label` occurrence's scope, allocating its unique slot —
    /// the allocation delegates to [`Self::allocate_anonymous`], exactly as
    /// `Scopes::push` delegates to `Scopes::allocate_anonymous`.
    fn push(&mut self, name: &str) -> Result<LabelSlot, EngineCompileError> {
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
    fn allocate_anonymous(&mut self) -> Result<LabelSlot, EngineCompileError> {
        let slot = self.next_slot;
        self.next_slot = slot.checked_add(1).ok_or_else(|| {
            EngineCompileError::Parse(ParseRejection::internal(
                "program exceeds the label slot addressing bound",
            ))
        })?;
        Ok(slot)
    }

    /// Pops the innermost `label` scope, ending its lexical extent.
    fn pop(&mut self) {
        self.entries.pop();
    }

    /// The slot the innermost enclosing `label` of `name` owns, or `None` when
    /// the label is not in lexical scope (the `$*label-x is not defined`).
    fn resolve(&self, name: &str) -> Option<LabelSlot> {
        self.entries
            .iter()
            .rev()
            .find(|(bound, _)| bound == name)
            .map(|(_, slot)| *slot)
    }
}

/// How one `def` parameter is passed.
enum ParamBinding<'ast> {
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
struct DefEntry<'ast> {
    name: String,
    arity: usize,
    params: &'ast [jqf_syntax::DefParameter],
    body: &'ast Expr,
    /// The source the body's spans index into.
    ///
    /// A definition and its call site need not come from the same text: the
    /// standard-library prelude is its own parse, and inlining one of its
    /// definitions into a user program must resolve the body's spans against
    /// the PRELUDE. `SyntaxSource` is `Copy`, so carrying it per definition
    /// costs nothing.
    source: SyntaxSource<'ast>,
    var_depth: usize,
    label_depth: usize,
    def_depth: usize,
    /// Set while this definition's own body is being lowered. A call reaching an
    /// active definition is RECURSION, which routes to the callable path.
    active: bool,
    /// The compiled callable body's arena slot, once the definition has been
    /// compiled for recursive calls (`None` until then). Every call site shares
    /// the one compiled body; the recursion depth is bounded at run time.
    /// Filter parameters are runtime closures bound at each call, so a
    /// filter-parameter definition uses this same single body rather than a
    /// per-argument specialization.
    callable: Option<usize>,
}

/// One def exposed by a loaded module: a pre-compiled callable body under an
/// exposed name (plain for `include`, `alias::name` for `import`).
struct ModuleDefEntry {
    name: String,
    arity: usize,
    callable: usize,
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
    /// Owned parse tree; bound for the rest of the compile in [`compile`].
    pub syntax: jqf_syntax::ParsedSyntax<jqf_syntax::SourceUnit>,
}

/// One prepared module bound to its retained text for the duration of lowering.
pub(crate) struct BoundModule<'tree, 'source> {
    pub label: &'source str,
    pub dir: &'source str,
    pub bound: jqf_syntax::BoundSyntax<'tree, 'source, jqf_syntax::SourceUnit>,
}

/// Everything one loaded module's lowering produced, ready to merge into the
/// parent lowerer's arena.
struct ModuleLowering<'ast> {
    nodes: Vec<ProgramNode>,
    callables: Vec<CallableDef>,
    /// The defs the module exposes to the importer (plain or `alias::name`),
    /// with callable indexes into `callables`.
    exposed: Vec<ModuleDefEntry>,
    /// Filter-parameter defs: exported as ordinary [`DefEntry`]s so a later
    /// call site inlines them with the call-by-name law.
    filter_defs: Vec<DefEntry<'ast>>,
    slots: u32,
    engine_slots: u32,
    labels: u32,
    /// Whether the module bound the `~inputs` resident; merged into the parent
    /// so the compile result's null-first scoping covers module defs too.
    uses_inputs_cursor: bool,
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
const MAX_LOWERED_NODES: usize = 200_000;

/// Lowering state: the arena under construction, the two lexical scope stacks
/// (variables and labels are separate namespaces), the visible `def` stack, and
/// the parameter bindings of the definition currently being inlined.
struct Lowerer<'ast, 'resources> {
    nodes: Vec<ProgramNode>,
    scopes: Scopes,
    /// The ENGINE-binding scope stack: `as ~x` binders push one entry naming
    /// the cursor and the machine slot it owns; a `~x` pull resolves against it.
    /// Structurally identical to [`Scopes`] and a SEPARATE namespace from it —
    /// `~x` and `$x` never collide.
    engine_scopes: EngineScopes,
    labels: LabelScopes,
    defs: Vec<DefEntry<'ast>>,
    callables: Vec<CallableDef>,
    /// Defs loaded from modules, in exposure order (later entries shadow).
    module_defs: Vec<ModuleDefEntry>,
    /// Data-import variables (`$alias` → the module's data array), in order.
    module_vars: Vec<(String, Value)>,
    /// `--arg`/`--argjson` bindings: names WITH the `$` prefix, later bindings
    /// shadowing earlier ones. Consulted only after every lexical scope has
    /// failed (a program binder always wins over a CLI binding) and after
    /// `module_vars` (a data import pre-binds its `$alias`). A reference
    /// matching neither the scopes nor this table stays the `$x is not
    /// defined`. Borrowed, not owned: `Value` is deliberately not
    /// `Clone`, and the bindings outlive the lowerer by construction.
    cli_vars: &'resources [(String, Value)],
    params: Vec<ParamBinding<'ast>>,
    /// Syntax levels open on [`lower_expr`]'s own call stack.
    ///
    /// The parser bounds the tree it BUILDS, but lowering reaches trees the
    /// parser never saw — a `def` body is re-lowered at every call site, and an
    /// inlined filter argument is re-lowered inside the callee — so the walk
    /// carries its own counter against the same ceiling rather than trusting
    /// the shape of the input tree.
    depth: u32,
    /// The request context lowering runs under: the nesting ceiling its walk
    /// reads, the module loader and data-import decode it drives, and the
    /// ledger that lets an allocation refusal surface as a resource error.
    ///
    /// Lowering does NOT charge this account — no compile-time literal or
    /// arena charge exists; a literal's residency is carried by the compiled
    /// program itself.
    resources: &'resources ResourceContext<'resources>,
    /// Provably-constant lexical bindings in scope, keyed by slot: `1 as $x |
    /// .[$x:]` folds the slice bound to the literal. An entry lives exactly
    /// while its binder's BODY lowers and is popped when the scope closes, so
    /// a slot is never read as constant outside the binder that made it so.
    const_bindings: Vec<(VarSlot, Value)>,
    /// Recursive-CALLABLE bodies currently being lowered (nested `def`s inside
    /// a recursive definition's body). A pull of an engine binding from inside
    /// one is the carve-out: the callable body runs on a NESTED evaluator
    /// with no cursor store, so the pull is rejected at lower time with a typed
    /// error naming the restriction (the recursive-def loop idiom is written
    /// with `while`/`repeat`/`recurse` instead).
    callable_depth: usize,
    /// A `~generator` constructor's argument graphs currently being lowered. A
    /// pull of an engine binding inside one is CROSS-MACHINE capture: the
    /// constructor's cursor is a separately-owned machine whose graphs cannot
    /// pull a cursor the ENCLOSING machine owns, so the pull is rejected at
    /// lower time rather than failing on an empty cursor slot at run time.
    in_engine_constructor: usize,
    /// Next unused filter-parameter slot. Unique per recursive-callable
    /// filter-parameter occurrence, the way variable slots are unique per
    /// binder occurrence.
    next_filter_slot: crate::program::FilterSlot,
    /// Whether this lowering bound the `~inputs` resident: the
    /// input-sequence cursor is scoped to the null-first drive (a cursor over
    /// the input sequence collides with the per-element cursor-store reset),
    /// and the flag is how the compile result carries that scoping to the
    /// route planner.
    uses_inputs_cursor: bool,
    /// Whether this compile is the SPLIT lane's (`--split-exp`):
    /// the only mode that resolves an unbound `$index` reference into a
    /// runtime variable slot instead of reporting `$index is not defined`. The
    /// split expression runs once per published item with the item counter
    /// bound to that slot. Off for every ordinary compile, so a user program
    /// that references an unbound `$index` keeps the ordinary error.
    runtime_index: bool,
    /// The slot `$index` lowered to under the split lane, recorded once at the
    /// first reference. `None` when the expression never references it.
    runtime_index_slot: Option<VarSlot>,
}

/// What a lowering entry point returns: the arena, its root, the binder-occurrence
/// slot count, the ENGINE-cursor slot count, and the compiled callables.
type Lowered = (
    Vec<ProgramNode>,
    ProgramNodeId,
    u32,
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
/// The third component of the result is the program's binder-occurrence SLOT
/// COUNT — the size the executor gives its env vector once, at machine seed. It
/// is zero for a binder-free program.
///
/// Production callers all go through [`lower_with_prelude`], since the stdlib is
/// always in scope for a real compile; this prelude-free spelling exists so the
/// lowering tests can assert on an arena built from the user program alone.
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
#[cfg(test)]
pub(crate) fn lower_with_prelude<'ast>(
    preludes: &[(&'ast Expr, &'ast SyntaxSource<'ast>)],
    expr: &'ast Expr,
    source: &SyntaxSource<'ast>,
    cli_vars: &[(String, Value)],
    resources: &ResourceContext<'_>,
    runtime_index: bool,
) -> Result<Lowered, EngineCompileError> {
    let mut lowerer = Lowerer {
        nodes: Vec::new(),
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
    };
    for (prelude_root, prelude_source) in preludes {
        push_prelude_definitions(prelude_root, prelude_source, &mut lowerer)?;
    }
    let root = lower_expr(expr, source, &mut lowerer)?;
    Ok((
        lowerer.nodes,
        root,
        lowerer.scopes.next_slot,
        lowerer.engine_scopes.next_slot.0,
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
    modules: &[BoundModule<'ast, 'ast>],
    cli_vars: &[(String, Value)],
    resources: &ResourceContext<'_>,
    runtime_index: bool,
) -> Result<Lowered, EngineCompileError> {
    let mut lowerer = Lowerer {
        nodes: Vec::new(),
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
    };
    for (prelude_root, prelude_source) in preludes {
        push_prelude_definitions(prelude_root, prelude_source, &mut lowerer)?;
    }
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
                let exposed = process_import(item, source, &mut lowerer, None, modules, preludes)?;
                register_exposed_defs(&mut lowerer, exposed);
            }
            SourceItem::Include(item) => {
                let exposed = process_include(item, source, &mut lowerer, None, modules, preludes)?;
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
        lowerer.engine_scopes.next_slot.0,
        lowerer.callables,
        lowerer.uses_inputs_cursor,
        lowerer.runtime_index_slot,
    ))
}

/// Pushes one top-level user definition onto the visible stack.
fn push_def_entry<'ast>(
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
fn prepare_one_module(
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
        return Ok(());
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
    let parsed = jqf_syntax::parse_library(source_ref, &resolved.text).map_err(EngineCompileError::Input)?;
    let syntax = parsed
        .into_valid_syntax()
        .map_err(|diagnostics| EngineCompileError::Parse(ParseRejection::from_diagnostics(&diagnostics)))?;
    let bound_resolved = jqf_source::ResolvedSource::new(source_ref, &resolved.label, resolved.text.as_bytes(), 0);
    let bound = syntax
        .bind(bound_resolved)
        .map_err(|error| EngineCompileError::Parse(ParseRejection::from_bind(error)))?;
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
fn register_exposed_defs(lowerer: &mut Lowerer<'_, '_>, exposed: Vec<ModuleDefEntry>) {
    for entry in exposed {
        lowerer.module_defs.push(entry);
    }
}

/// Registers one module def under a plain internal name (visible to the
/// module's own later defs, removed again when the module arena is merged).
fn register_module_def(lowerer: &mut Lowerer<'_, '_>, name: &str, arity: usize, callable: usize) {
    lowerer.module_defs.push(ModuleDefEntry {
        name: copy_string(name).expect("module def name is copyable"),
        arity,
        callable,
    });
}

/// Processes one `import` item: resolves the module (or data file) and returns
/// the defs it exposes to the importing scope.
fn process_import<'ast>(
    item: &'ast ImportItem,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
    lib_origin: Option<&str>,
    modules: &[BoundModule<'ast, 'ast>],
    preludes: &[(&'ast Expr, &'ast SyntaxSource<'ast>)],
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
    let module = lower_bound_module(
        &resolved,
        Some(alias_text),
        lowerer.cli_vars,
        lowerer.resources,
        modules,
        preludes,
    )?;
    merge_module(lowerer, module)
}

/// Processes one `include` item: resolves the module and returns its defs under
/// their PLAIN names.
fn process_include<'ast>(
    item: &'ast IncludeItem,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
    lib_origin: Option<&str>,
    modules: &[BoundModule<'ast, 'ast>],
    preludes: &[(&'ast Expr, &'ast SyntaxSource<'ast>)],
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
    let module = lower_bound_module(&resolved, None, lowerer.cli_vars, lowerer.resources, modules, preludes)?;
    merge_module(lowerer, module)
}

/// Looks up a prepared module by resolved label and lowers it.
fn lower_bound_module<'ast>(
    loaded: &crate::exec::LoadedModule,
    prefix: Option<&str>,
    cli_vars: &[(String, Value)],
    resources: &ResourceContext<'_>,
    modules: &[BoundModule<'ast, 'ast>],
    preludes: &[(&'ast Expr, &'ast SyntaxSource<'ast>)],
) -> Result<ModuleLowering<'ast>, EngineCompileError> {
    let prepared = modules.iter().find(|module| module.label == loaded.label);
    let Some(prepared) = prepared else {
        return Err(EngineCompileError::Parse(ParseRejection::internal(
            "prepared module catalog missed a resolved library",
        )));
    };
    lower_module(
        prepared.bound.root(),
        prepared.bound.source(),
        prepared.dir,
        prefix,
        cli_vars,
        resources,
        modules,
        preludes,
    )
}

/// Lowers one loaded module in its OWN lowerer, returning the arena, callables,
/// and the defs it exposes.
///
/// Value-parameter defs compile once as callables (the historical path).
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
fn lower_module<'ast>(
    unit: &'ast jqf_syntax::SourceUnit,
    module_source: &SyntaxSource<'ast>,
    dir: &str,
    prefix: Option<&str>,
    cli_vars: &[(String, Value)],
    resources: &ResourceContext<'_>,
    modules: &[BoundModule<'ast, 'ast>],
    preludes: &[(&'ast Expr, &'ast SyntaxSource<'ast>)],
) -> Result<ModuleLowering<'ast>, EngineCompileError> {
    let mut lowerer = Lowerer {
        nodes: Vec::new(),
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
        runtime_index: false,
        runtime_index_slot: None,
    };
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
                let exposed = process_import(import, module_source, &mut lowerer, Some(dir), modules, preludes)?;
                register_exposed_defs(&mut lowerer, exposed);
            }
            SourceItem::Include(include) => {
                let exposed = process_include(include, module_source, &mut lowerer, Some(dir), modules, preludes)?;
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
                    register_module_def(&mut lowerer, name, arity, callable);
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
    })
}

/// Whether any parameter is a call-by-name filter (an undecorated identifier).
fn def_has_filter_parameter(params: &[DefParameter], source: &SyntaxSource<'_>) -> bool {
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
fn merge_module<'ast>(
    lowerer: &mut Lowerer<'ast, '_>,
    module: ModuleLowering<'ast>,
) -> Result<Vec<ModuleDefEntry>, EngineCompileError> {
    let node_base = lowerer.nodes.len();
    let callable_base = lowerer.callables.len();
    let slot_base = lowerer.scopes.next_slot;
    let engine_slot_base = lowerer.engine_scopes.next_slot.0;
    let label_base = lowerer.labels.next_slot;
    // A module def body may bind `~inputs`; the resident's null-first scoping
    // travels with the module into the merged program.
    lowerer.uses_inputs_cursor |= module.uses_inputs_cursor;
    for mut node in module.nodes {
        rebase_node(&mut node, node_base, slot_base, engine_slot_base, label_base);
        lowerer.nodes.push(node);
    }
    for mut callable in module.callables {
        callable.body = ProgramNodeId::from_index(callable.body.index() + node_base)
            .expect("module arena stays within the addressing bound");
        for slot in &mut callable.param_slots {
            *slot += slot_base;
        }
        lowerer.callables.push(callable);
    }
    let mut adjusted = Vec::new();
    adjusted
        .try_reserve_exact(module.exposed.len())
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    for mut entry in module.exposed {
        entry.callable += callable_base;
        lowerer.module_defs.push(ModuleDefEntry {
            name: entry.name.clone(),
            arity: entry.arity,
            callable: entry.callable,
        });
        adjusted.push(entry);
    }
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
    Ok(adjusted)
}

/// Rebases every arena edge, binder slot, and label slot in one node from a
/// module arena into the parent's numbering.
#[allow(
    clippy::too_many_lines,
    reason = "one arm per arena node family: the rebase walk is read as a single table"
)]
fn rebase_node(node: &mut ProgramNode, node_base: usize, slot_base: u32, engine_slot_base: u32, label_base: u32) {
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
            args,
            filter_args,
            ..
        } => {
            rebase_id(body);
            for slot in param_slots {
                *slot += slot_base;
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
        ProgramNode::Empty | ProgramNode::CallFilter { .. } => {}
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
        ProgramNode::Modify { paths, update, .. } | ProgramNode::FactAssign { paths, update, .. } => {
            rebase_id(paths);
            rebase_id(update);
        }
    }
}

/// Compiles one module def's body ONCE as a callable. Value parameters bind
/// ordinary slots. Filter-parameter defs never enter here: they stay as
/// [`DefEntry`]s over the prepared module AST so a later call site inlines
/// them with the call-by-name law.
fn compile_module_callable<'ast>(
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
fn constant_metadata<'ast>(
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
fn metadata_search(metadata: Option<&Value>) -> Option<Vec<String>> {
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
/// the one shape module metadata never sees — `[1,2]` is executable choice
/// whose elements concatenate in evaluation order — so the fold walks it as a
/// sequence. Returns `None` for any non-constant shape AND for a resource
/// failure: both fall back to the ordinary per-element path, which reports the
/// allocation when it matters.
///
/// The constant-result folding — whole constant containers lower to one
/// literal producer — is the mechanism; this is the same idea lowered through
/// the literal producer.
fn try_fold_constant_array<'ast>(
    expression: Option<&'ast Expr>,
    source: &SyntaxSource<'ast>,
) -> Result<Option<Value>, EngineCompileError> {
    let mut values = Vec::new();
    if let Some(generator) = expression
        && !try_fold_constant_seq(generator, source, &mut values)?
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

/// Appends the constant values `expr` evaluates to, in evaluation order.
/// Returns `false` (appending nothing) when `expr` is not provably constant.
fn try_fold_constant_seq<'ast>(
    expr: &'ast Expr,
    source: &SyntaxSource<'ast>,
    out: &mut Vec<Value>,
) -> Result<bool, EngineCompileError> {
    if let ExprKind::Binary(binary) = expr.kind()
        && binary.op == BinaryOp::Comma
    {
        if !try_fold_constant_seq(&binary.left, source, out)? {
            return Ok(false);
        }
        return try_fold_constant_seq(&binary.right, source, out);
    }
    match evaluate_constant(expr, source) {
        Ok(value) => {
            out.push(value);
            Ok(true)
        }
        // A non-constant shape (and a resource failure) declines the fold.
        Err(_) => Ok(false),
    }
}

/// Best-effort constant fold of an object constructor for the MAIN lowering
/// path: `{a:1, b:2}` lowers to one literal producer instead of per-element
/// `ConstructObject` construction. The object builds through the SAME
/// `ObjectBuilder` law as runtime construction, so the first-duplicate-fixes-
/// position / final-occurrence-supplies-the-value law is inherited verbatim.
/// Returns `None` for any member that is not provably constant (dynamic or
/// interpolated keys, shorthand members, non-constant values).
fn try_fold_constant_object<'ast>(
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

/// One constant object-constructor key as an owned object key.
/// Walks a prelude's `def a: …; def b: …; .` chain, making every definition
/// visible without lowering any of them.
///
/// Nothing is lowered here on purpose: an unused stdlib definition must cost
/// zero arena nodes, so a program that calls none of them lowers exactly as it
/// did before the prelude existed.
fn push_prelude_definitions<'ast>(
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
fn lower_definition<'ast>(
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
fn lower_user_call<'ast>(
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
fn try_lower_zero_arg_user<'ast>(
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
fn emit_module_call<'ast>(
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
fn lower_filter_argument(position: usize, lowerer: &mut Lowerer<'_, '_>) -> Result<ProgramNodeId, EngineCompileError> {
    if lowerer.nodes.len() > MAX_LOWERED_NODES {
        return Err(EngineCompileError::Parse(ParseRejection::internal(
            "function expansion exceeded the inlining bound",
        )));
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
fn clone_defs<'ast>(entries: &[DefEntry<'ast>]) -> Result<Vec<DefEntry<'ast>>, EngineCompileError> {
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
fn clone_scope<S: Copy>(entries: &[(String, S)]) -> Result<Vec<(String, S)>, EngineCompileError> {
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
fn capture_call_arguments<'ast>(
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
fn inline_definition<'ast>(
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
    if lowerer.nodes.len() > MAX_LOWERED_NODES {
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
/// against the slot bound into the shared body. That is jq's call-by-name
/// law — each use evaluates the captured graph against the captured env —
/// and it is why a self-call `f(n-1)` does not freeze `n` at the first
/// recursive argument.
fn lower_callable<'ast>(
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
        // [`resolve_call_defs`] rewrites the node with the real body id.
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
fn lower_callable_body<'ast>(
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
                def_base: 0,
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

/// Whether any node in the subgraph rooted at `id` contains a `.[]`
/// ([`StepAccess::Each`]) step — the count-fusion gate (see the pipe arm).
fn graph_has_each_step(nodes: &[ProgramNode], id: ProgramNodeId) -> bool {
    fn walk(nodes: &[ProgramNode], id: ProgramNodeId, seen: &mut [bool]) -> bool {
        let index = id.index();
        if index >= seen.len() || seen[index] {
            return false;
        }
        seen[index] = true;
        match &nodes[index] {
            ProgramNode::Stage { steps, .. } => steps
                .iter()
                .any(|step| matches!(step.access(), StepAccess::Each)),
            ProgramNode::FlatMap { upstream, body }
            | ProgramNode::Binary {
                left: upstream,
                right: body,
                ..
            } => walk(nodes, *upstream, seen) || walk(nodes, *body, seen),
            ProgramNode::Concat { parts } => parts.iter().any(|part| walk(nodes, *part, seen)),
            ProgramNode::Conditional {
                condition,
                consequent,
                alternative,
            } => {
                walk(nodes, *condition, seen)
                    || walk(nodes, *consequent, seen)
                    || walk(nodes, *alternative, seen)
            }
            ProgramNode::Choice { left, right }
            | ProgramNode::Alternative { left, right }
            | ProgramNode::Logical { left, right, .. } => {
                walk(nodes, *left, seen) || walk(nodes, *right, seen)
            }
            ProgramNode::CollectArray { body } | ProgramNode::CountCollect { body } => {
                body.is_some_and(|body| walk(nodes, body, seen))
            }
            ProgramNode::ConstructObject { members } => members
                .iter()
                .any(|member| walk(nodes, member.key, seen) || walk(nodes, member.value, seen)),
            ProgramNode::Call { args, .. } => args.iter().any(|arg| walk(nodes, *arg, seen)),
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
            ProgramNode::EngineGenerator {
                init,
                update,
                extract,
            } => {
                walk(nodes, *init, seen)
                    || walk(nodes, *update, seen)
                    || walk(nodes, *extract, seen)
            }
            ProgramNode::EngineRng { seed } => walk(nodes, *seed, seen),
            ProgramNode::Bind { source, body, .. }
            | ProgramNode::EngineBind { source, body, .. } => {
                walk(nodes, *source, seen) || walk(nodes, *body, seen)
            }
            ProgramNode::Reduce {
                source,
                init,
                update,
                ..
            } => {
                walk(nodes, *source, seen) || walk(nodes, *init, seen) || walk(nodes, *update, seen)
            }
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
            ProgramNode::Try { body, handler } => {
                walk(nodes, *body, seen) || handler.is_some_and(|h| walk(nodes, h, seen))
            }
            ProgramNode::ChainBody { body } | ProgramNode::Label { body, .. } => {
                walk(nodes, *body, seen)
            }
            ProgramNode::Modify { paths, update, .. }
            | ProgramNode::FactAssign { paths, update, .. } => {
                walk(nodes, *paths, seen) || walk(nodes, *update, seen)
            }
            ProgramNode::Empty
            | ProgramNode::CallFilter { .. }
            | ProgramNode::Break { .. }
            // An engine pull holds no children to walk.
            | ProgramNode::EnginePull { .. } => false,
            ProgramNode::Counted { source, .. } => walk(nodes, *source, seen),
        }
    }
    walk(nodes, id, &mut vec![false; nodes.len()])
}

/// Lowers one subset expression into the arena, returning its root node id.
///
/// Pipe recurses on both sides (right-associative) and emits a `FlatMap`; comma
/// recurses and emits a `Choice` (left-associative via the parser tree); a group
/// `(expr)` lowers transparently to `expr`'s graph; identity and static
/// paths/iteration emit a `Stage`, with a postfix chain on a group base
/// composing onto that base. Every other form is rejected by name and span.
fn lower_expr<'ast>(
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
fn lower_expr_at_depth<'ast>(
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
                    // binder shadows it exactly as jq shadows an import
                    // (`import "d" as $d; 1 as $d | $d` answers `1`). Later
                    // aliases shadow earlier ones.
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
                                let slot = lowerer.scopes.allocate_anonymous()?;
                                lowerer.runtime_index_slot = Some(slot);
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
                    && !graph_has_each_step(&lowerer.nodes, collect_body)
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
        ExprKind::Error | ExprKind::Unary(_) | ExprKind::Binary(_) => Err(EngineCompileError::unsupported(
            expr.span(),
            describe_expr_kind(expr.kind()),
        )),
    }
}

/// Whether a variable reference names the `$__loc__` location binding.
fn is_loc_binding(span: Span, source: &SyntaxSource<'_>) -> bool {
    source.text().get(span.range()).is_some_and(|name| name == "$__loc__")
}

/// Whether `span` names the `$ENV` environment binding.
///
/// `$ENV` is not an ordinary binder slot: it lowers to the `env/0` call
/// everywhere it appears — a bare `$ENV`, `{$ENV}`, `.[$ENV]` and `.[$ENV:]` —
/// so the three sites that resolve variables directly must agree with the
/// lowered path.
fn is_env_variable(span: Span, source: &SyntaxSource<'_>) -> bool {
    source.text().get(span.range()).is_some_and(|name| name == "$ENV")
}

/// The `env/0` call `$ENV` lowers to — the ONE law shared by every spelling
/// of the environment binding. `$ENV` is PRE-bound, so a `def env: …;` shadows
/// the builtin NAME for ordinary calls but never this binding (`def env: 1;
/// $ENV` answers the environment object).
fn lower_env_call(lowerer: &mut Lowerer<'_, '_>) -> Result<ProgramNodeId, EngineCompileError> {
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
fn location_literal(
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
fn try_lower_loc_binding(
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
fn resolve_variable(span: Span, source: &SyntaxSource<'_>, scopes: &Scopes) -> Result<VarSlot, EngineCompileError> {
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

/// The variable text (`$x`, including the sigil) a `Variable` pattern binds.
fn pattern_variable<'text>(
    pattern: &Pattern,
    source: &'text SyntaxSource<'_>,
) -> Result<&'text str, EngineCompileError> {
    source
        .text()
        .get(pattern.span().range())
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("pattern span out of range")))
}

/// One binder a pattern contributes: the slot it owns, the graph that fills it,
/// and the name it opens under — `None` for the intermediate values a nested
/// pattern needs, which no user source can reference.
struct PatternBinding {
    name: Option<String>,
    slot: VarSlot,
    source: ProgramNodeId,
}

/// Collects `pattern`'s extraction frame over `value`, a graph producing the
/// matched value.
///
/// The frame is a FLAT left-to-right list of binders, nested by
/// [`wrap_pattern_frame`] in that order, and it carries every law in one shape:
///
/// * an array element is `$matched | .[i]`, so every error is that index step's
///   own (`{"a":1} | . as [$a] | $a` is "Cannot index object with number (0)",
///   and `null | . as [$a,$b] | [$a,$b]` is `[null,null]`);
/// * a repeated name is two DISTINCT slots with the later shadowing in the body
///   (`[1,2] | . as [$a,$a] | $a` is `2`), which the one-slot-per-occurrence law
///   gives for free;
/// * a nested pattern binds its own anonymous intermediate, so `. as [$a,[$b]]`
///   reports `.[1][0]`'s failure and not a pattern-shape complaint;
/// * NO name is opened here. Visibility is the caller's step, after the whole
///   frame exists — see [`Scopes::open`].
fn collect_pattern_bindings<'ast>(
    pattern: &'ast Pattern,
    value: ProgramNodeId,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
    bindings: &mut Vec<PatternBinding>,
    named: &alloc::collections::BTreeMap<String, VarSlot>,
) -> Result<(), EngineCompileError> {
    match pattern.kind() {
        PatternKind::Variable => {
            let name = copy_string(pattern_variable(pattern, source)?)?;
            record_binding(Some(name), value, lowerer, bindings, named).map(|_| ())
        }
        PatternKind::Array(elements) => {
            let matched = record_binding(None, value, lowerer, bindings, named)?;
            // The frame binds RIGHT-to-LEFT: `"a" as [$v0, $v1]` fails at
            // `.[1]`, so the LAST element's step runs first. The slots and the
            // body's names are unaffected — only which step raises first when
            // several would.
            for (position, element) in elements.iter().enumerate().rev() {
                let index = i64::try_from(position).map_err(|_| {
                    EngineCompileError::Parse(ParseRejection::internal(
                        "array pattern exceeds the index addressing bound",
                    ))
                })?;
                let member = variable_step(matched, StepAccess::Index(index), lowerer)?;
                collect_pattern_bindings(element, member, source, lowerer, bindings, named)?;
            }
            Ok(())
        }
        PatternKind::Object(members) => {
            let matched = record_binding(None, value, lowerer, bindings, named)?;
            for member in members {
                collect_member_bindings(member, matched, source, lowerer, bindings, named)?;
            }
            Ok(())
        }
        // `?//` is not a pattern POSITION — the grammar allows it only at the top
        // level of an as-clause, and jqf-syntax enforces the same — so reaching it
        // here means a caller forgot to split the chain first.
        PatternKind::Alternative(_, _) => Err(EngineCompileError::Parse(ParseRejection::internal(
            "a `?//` chain reached the pattern frame builder",
        ))),
        PatternKind::Error => Err(EngineCompileError::unsupported(
            pattern.span(),
            UnsupportedConstruct::Expression("a recovered syntax error"),
        )),
        PatternKind::EngineBinding => Err(EngineCompileError::unsupported(
            pattern.span(),
            UnsupportedConstruct::Expression("an unsupported binding pattern"),
        )),
    }
}

/// Flattens a right-associative `?//` chain into its alternatives, in source
/// order. A pattern with no `?//` flattens to itself.
fn flatten_alternatives<'ast>(
    pattern: &'ast Pattern,
    alternatives: &mut Vec<&'ast Pattern>,
) -> Result<(), EngineCompileError> {
    if let PatternKind::Alternative(left, right) = pattern.kind() {
        flatten_alternatives(left, alternatives)?;
        return flatten_alternatives(right, alternatives);
    }
    alternatives
        .try_reserve(1)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    alternatives.push(pattern);
    Ok(())
}

/// Every variable name a pattern binds, deduplicated, in source order.
///
/// It mirrors [`collect_pattern_bindings`]'s named binders exactly — a `Variable`
/// position and an object member's `$`-key shorthand — because a `?//` arm has to
/// bind the UNION of the chain's names and the difference is what it fills with
/// `null`.
fn collect_pattern_names(
    pattern: &Pattern,
    source: &SyntaxSource<'_>,
    names: &mut Vec<String>,
) -> Result<(), EngineCompileError> {
    match pattern.kind() {
        PatternKind::Variable => record_name(pattern_variable(pattern, source)?, names),
        PatternKind::Array(elements) => {
            for element in elements {
                collect_pattern_names(element, source, names)?;
            }
            Ok(())
        }
        PatternKind::Object(members) => {
            for member in members {
                if let ObjectKey::Variable(span) = &member.key {
                    let text = source.text().get(span.range()).ok_or_else(|| {
                        EngineCompileError::Parse(ParseRejection::internal(
                            "object `$var` pattern key span out of range",
                        ))
                    })?;
                    record_name(text, names)?;
                }
                if let Some(nested) = &member.pattern {
                    collect_pattern_names(nested, source, names)?;
                }
            }
            Ok(())
        }
        PatternKind::Alternative(left, right) => {
            collect_pattern_names(left, source, names)?;
            collect_pattern_names(right, source, names)
        }
        PatternKind::Error | PatternKind::EngineBinding => Ok(()),
    }
}

/// Appends `name` unless it is already recorded.
fn record_name(name: &str, names: &mut Vec<String>) -> Result<(), EngineCompileError> {
    if names.iter().any(|recorded| recorded == name) {
        return Ok(());
    }
    names
        .try_reserve(1)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    names.push(copy_string(name)?);
    Ok(())
}

/// Builds one `?//` alternative's frame and opens its scope, answering the frame
/// and how many extents the caller must close.
///
/// The arm binds the CHAIN's whole variable union: its own pattern's names by
/// extraction, and every other alternative's names from a `null` literal. That is
/// a lower-time fact and not a runtime clear — `[1] | . as {a:$x} ?// [$y] |
/// [$x,$y]` is `[null,1]` because `$x`'s binder is a `null` source in that arm.
fn open_alternative_arm<'ast>(
    pattern: &'ast Pattern,
    matched: VarSlot,
    union: &[String],
    named: &alloc::collections::BTreeMap<String, VarSlot>,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<(Vec<PatternBinding>, usize), EngineCompileError> {
    let mut frame = Vec::new();
    let value = push_node(&mut lowerer.nodes, variable_stage(matched))?;
    collect_pattern_bindings(pattern, value, source, lowerer, &mut frame, named)?;
    let mut own = Vec::new();
    collect_pattern_names(pattern, source, &mut own)?;
    for name in union {
        if own.iter().any(|bound| bound == name) {
            continue;
        }
        let null = push_node(&mut lowerer.nodes, literal_stage(Value::Null))?;
        record_binding(Some(copy_string(name)?), null, lowerer, &mut frame, named)?;
    }
    let opened = open_pattern_scope(&frame, lowerer)?;
    Ok((frame, opened))
}

/// Chains built arms into `Try{arm1, handler: Try{arm2, handler: arm3}}`.
///
/// Four laws come out of the shape. The barrier covers the arm's
/// extraction AND its body, because both are inside the `Try` body. Values the
/// failing arm already emitted STAND, because a `Try` body's emissions pass
/// through and only the raise routes to the handler (`[1] | [. as [$y] ?// $z |
/// (1, if $y then error("boom") else 2 end)]` is `[1,1,2]` — the leading `1`
/// twice, which is also the proof the body re-runs WHOLE). The LAST alternative
/// has no enclosing handler, so its failure escapes, which is why a trailing `$z`
/// makes a chain total. And a handler runs with dot = the RAISED value, so every
/// arm after the first re-reads the incoming dot out of `dot`.
fn chain_alternatives(
    arms: &[ProgramNodeId],
    dot: VarSlot,
    nodes: &mut Vec<ProgramNode>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let (first, rest) = arms
        .split_first()
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("a `?//` chain built no arms")))?;
    let mut chain: Option<ProgramNodeId> = None;
    for arm in rest.iter().rev() {
        let incoming = push_node(nodes, variable_stage(dot))?;
        let restored = push_node(
            nodes,
            ProgramNode::FlatMap {
                upstream: incoming,
                body: *arm,
            },
        )?;
        chain = Some(match chain {
            None => restored,
            Some(handler) => push_node(
                nodes,
                ProgramNode::Try {
                    body: restored,
                    handler: Some(handler),
                },
            )?,
        });
    }
    match chain {
        None => Ok(*first),
        Some(handler) => push_node(
            nodes,
            ProgramNode::Try {
                body: *first,
                handler: Some(handler),
            },
        ),
    }
}

/// Collects one object-pattern member's binders.
///
/// The five key forms, and two of them carry laws of their own. A
/// COMPUTED key (`{(e): P}`, `{"k\(e)": P}`) is lowered with dot = the MATCHED
/// VALUE at this nesting level, not the program's input (`{"a":{"k":"x","x":5}} |
/// . as {a: {(.k): $v}} | $v` is `5`), and it is a GENERATOR whose every output is
/// its own binding set (`{"a":1,"b":2} | [. as {("a","b"):$x} | $x]` is `[1,2]`).
/// A `$`-key member (`{$b}`, `{$b: P}`) is TWO binders and not one: the key is the
/// identifier WITHOUT the sigil, `$b` binds that member's value, and a nested
/// pattern destructures the SAME value.
fn collect_member_bindings<'ast>(
    member: &'ast jqf_syntax::ObjectPatternMember,
    matched: VarSlot,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
    bindings: &mut Vec<PatternBinding>,
    named: &alloc::collections::BTreeMap<String, VarSlot>,
) -> Result<(), EngineCompileError> {
    let value = match &member.key {
        ObjectKey::Name(span) => {
            let key = identifier_text(*span, source)?;
            variable_step(matched, StepAccess::Key(key), lowerer)?
        }
        ObjectKey::String(template) => {
            if let Some(text) = static_template_text(template, source)? {
                variable_step(matched, StepAccess::Key(text), lowerer)?
            } else {
                let key = lower_string_template(template, source, lowerer)?;
                computed_member(matched, key, lowerer, bindings, named)?
            }
        }
        ObjectKey::Expr(expr) => {
            let key = lower_expr(expr.as_ref(), source, lowerer)?;
            computed_member(matched, key, lowerer, bindings, named)?
        }
        ObjectKey::Variable(span) => {
            let name = variable_key_text(*span, source)?;
            let value = variable_step(matched, StepAccess::Key(copy_string(name)?), lowerer)?;
            let sigiled = source.text().get(span.range()).ok_or_else(|| {
                EngineCompileError::Parse(ParseRejection::internal("object `$var` pattern key span out of range"))
            })?;
            let slot = record_binding(Some(copy_string(sigiled)?), value, lowerer, bindings, named)?;
            let Some(pattern) = &member.pattern else {
                return Ok(());
            };
            let same = push_node(&mut lowerer.nodes, variable_stage(slot))?;
            return collect_pattern_bindings(pattern, same, source, lowerer, bindings, named);
        }
        _ => {
            return Err(EngineCompileError::unsupported(
                member.span,
                UnsupportedConstruct::Expression("an unsupported object pattern key"),
            ));
        }
    };
    let Some(pattern) = &member.pattern else {
        return Err(EngineCompileError::unsupported(
            member.span,
            UnsupportedConstruct::Expression("an object pattern member without a binding"),
        ));
    };
    collect_pattern_bindings(pattern, value, source, lowerer, bindings, named)
}

/// The member-value graph for a COMPUTED key: the key expression runs with dot =
/// the matched value, binds to its own anonymous slot, and the member is that
/// slot's dynamic index into the same matched value.
fn computed_member(
    matched: VarSlot,
    key: ProgramNodeId,
    lowerer: &mut Lowerer<'_, '_>,
    bindings: &mut Vec<PatternBinding>,
    named: &alloc::collections::BTreeMap<String, VarSlot>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let dot = push_node(&mut lowerer.nodes, variable_stage(matched))?;
    let scoped = push_node(
        &mut lowerer.nodes,
        ProgramNode::FlatMap {
            upstream: dot,
            body: key,
        },
    )?;
    let slot = record_binding(None, scoped, lowerer, bindings, named)?;
    variable_step(matched, StepAccess::DynVar(slot), lowerer)
}

/// Records one binder on the frame, allocating its slot and answering it.
fn record_binding(
    name: Option<String>,
    source: ProgramNodeId,
    lowerer: &mut Lowerer<'_, '_>,
    bindings: &mut Vec<PatternBinding>,
    named: &alloc::collections::BTreeMap<String, VarSlot>,
) -> Result<VarSlot, EngineCompileError> {
    let slot = match &name {
        // A `?//` chain's union name uses the chain's ONE shared slot: every
        // arm binds the same slot before the shared body runs, so the body
        // reads the current arm's value from a stable identity.
        Some(name) => match named.get(name) {
            Some(slot) => *slot,
            None => lowerer.scopes.allocate_anonymous()?,
        },
        None => lowerer.scopes.allocate_anonymous()?,
    };
    bindings
        .try_reserve(1)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    bindings.push(PatternBinding { name, slot, source });
    Ok(slot)
}

/// A one-step stage reading `access` out of a bound slot.
fn variable_step(
    slot: VarSlot,
    access: StepAccess,
    lowerer: &mut Lowerer<'_, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let mut steps = Vec::new();
    steps
        .try_reserve_exact(1)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    steps.push(StageStep::new(access, false));
    push_node(
        &mut lowerer.nodes,
        ProgramNode::Stage {
            start: StageStart::Variable(slot),
            steps,
        },
    )
}

/// Opens every NAMED binder of a collected frame, answering how many extents to
/// close afterwards.
fn open_pattern_scope(bindings: &[PatternBinding], lowerer: &mut Lowerer<'_, '_>) -> Result<usize, EngineCompileError> {
    let mut opened = 0;
    // The frame collects RIGHT-to-left (the array pattern runs its LAST
    // element's step first), so the scope opens in the reverse of
    // the frame — SOURCE order — which is what makes a repeated name resolve
    // to its LAST source occurrence (`[1,2] | . as [$a,$a] | $a` is `2`).
    for binding in bindings.iter().rev() {
        if let Some(name) = &binding.name {
            lowerer.scopes.open(name, binding.slot)?;
            opened += 1;
        }
    }
    Ok(opened)
}

/// Closes the extents [`open_pattern_scope`] opened.
fn close_pattern_scope(opened: usize, lowerer: &mut Lowerer<'_, '_>) {
    for _ in 0..opened {
        lowerer.scopes.pop();
    }
}

/// Nests a collected frame around `body`, FIRST binder outermost.
///
/// Left-to-right nesting is what makes a computed key's generator the outer loop
/// of every member after it (`{"a":1,"b":2} | [. as {(("a","b")):$x, c:$y} |
/// [$x,$y]]` is `[[1,null],[2,null]]`).
fn wrap_pattern_frame(
    body: ProgramNodeId,
    bindings: Vec<PatternBinding>,
    nodes: &mut Vec<ProgramNode>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let mut current = body;
    let total = bindings.len();
    for (reversed, binding) in bindings.into_iter().rev().enumerate() {
        // The FIRST binder is the MATCHED value (the as-var), whose source is
        // frozen exactly like any ordinary bind's; every extraction binder
        // after it is a `frame` binder, whose source navigations path mode
        // evaluates with the register OPEN (the matcher indexes skip the
        // freeze).
        let frame = reversed + 1 < total;
        current = push_node(
            nodes,
            ProgramNode::Bind {
                source: binding.source,
                slot: binding.slot,
                body: current,
                frame,
            },
        )?;
    }
    Ok(current)
}

/// Lowers `label $out | BODY` into a [`ProgramNode::Label`].
///
/// The scope opens BEFORE the body and closes immediately after, so the label's
/// lexical extent is exactly the body — `(label $o | 1), break $o` is the
/// `$*label-o is not defined`, mirroring [`lower_binding`]'s discipline.
fn lower_label<'ast>(
    label: Span,
    body: &'ast Expr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let name = label_name(label, source)?;
    let slot = lowerer.labels.push(name)?;
    let body = lower_expr(body, source, lowerer);
    lowerer.labels.pop();
    let body = body?;
    push_node(&mut lowerer.nodes, ProgramNode::Label { slot, body })
}

/// The authored label text at `span` (including its `$`).
fn label_name<'source>(span: Span, source: &'source SyntaxSource<'_>) -> Result<&'source str, EngineCompileError> {
    source
        .text()
        .get(span.range())
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("label span out of range")))
}

/// Resolves `break $out` against the LABEL scope stack.
///
/// An unbound label is the compile-time `$*label-out is not defined` (exit-3
/// class) — a resolution failure like an unbound variable, not an out-of-subset
/// rejection. It reuses [`EngineCompileError::UndefinedLabel`] rather than
/// `UndefinedVariable` because the message names the `$*label-` namespace, and
/// conflating the two would misreport which stack the lookup missed.
fn resolve_label(span: Span, source: &SyntaxSource<'_>, labels: &LabelScopes) -> Result<LabelSlot, EngineCompileError> {
    let name = label_name(span, source)?;
    labels
        .resolve(name)
        .ok_or_else(|| EngineCompileError::undefined_label(span, name))
}

/// Lowers `SOURCE as PATTERN | BODY` into nested [`ProgramNode::Bind`]s.
///
/// The source is lowered BEFORE any name opens (a binding's source cannot see its
/// own variable), and the body inside them; the extents close immediately after,
/// so the binding's lexical extent is exactly the body (`(2 as $x | $x), $x` is a
/// compile error). A single-variable pattern is ONE `Bind` over the
/// source itself — exactly the pre-vertical shape — and a destructuring pattern
/// is the same rule applied to [`collect_pattern_bindings`]'s frame.
///
/// A `?//` chain becomes [`chain_alternatives`] over one arm per alternative, each
/// carrying its own copy of the BODY. The body copy is what the law requires, not
/// an expansion convenience: a raise anywhere in an arm restarts the NEXT
/// alternative from the beginning of the body.
fn lower_binding<'ast>(
    binding: &'ast jqf_syntax::BindingExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    // The ENGINE binding form `~CONSTRUCTOR as ~x | BODY`: the engine surface's
    // own binder, lexically scoped like `$x` but in the separate `~` namespace.
    if matches!(binding.pattern.kind(), PatternKind::EngineBinding) {
        return lower_engine_binding(binding, source, lowerer);
    }
    let source_id = lower_expr(&binding.value, source, lowerer)?;
    let mut alternatives = Vec::new();
    flatten_alternatives(&binding.pattern, &mut alternatives)?;
    if let [only] = alternatives.as_slice() {
        let mut frame = Vec::new();
        let named = alloc::collections::BTreeMap::new();
        collect_pattern_bindings(only, source_id, source, lowerer, &mut frame, &named)?;
        let opened = open_pattern_scope(&frame, lowerer)?;
        // A plain `$x` binding whose source is a lower-time constant
        // folds every `$x` slice bound inside the body to the literal. Only the
        // whole-value `Variable` pattern qualifies — a destructuring extraction
        // binds a PROJECTION of the source, not the source — and the entry is
        // popped with the scope, so the constant is never read outside the
        // binder that made it so (`1 as $x | .[0] as $x | .[$x:]` folds the
        // OUTER slice to `1` and leaves the inner slot dynamic).
        let constant = if frame.len() == 1
            && frame[0].name.is_some()
            && let Some(value) = fold_constant(&binding.value, source, lowerer.resources)?
        {
            Some((frame[0].slot, value))
        } else {
            None
        };
        if let Some((slot, value)) = &constant {
            lowerer.const_bindings.push((*slot, value.clone()));
        }
        let body = lower_expr(&binding.body, source, lowerer);
        if constant.is_some() {
            lowerer.const_bindings.pop();
        }
        close_pattern_scope(opened, lowerer);
        return wrap_pattern_frame(body?, frame, &mut lowerer.nodes);
    }
    // The incoming dot, bound OUTSIDE the chain so a handler arm can restore it.
    let incoming = push_node(&mut lowerer.nodes, current_stage())?;
    let dot = lowerer.scopes.allocate_anonymous()?;
    let matched = lowerer.scopes.allocate_anonymous()?;
    let union = alternative_union(&alternatives, source)?;
    // The chain binds the WHOLE name union with ONE slot per name: every arm
    // binds the same slots (its own names by extraction, the rest from null)
    // before the SHARED body runs, so the body reads the current arm's values
    // from stable identities. This is what makes one lowered body sound.
    let mut named = alloc::collections::BTreeMap::new();
    for name in &union {
        let slot = lowerer.scopes.allocate_anonymous()?;
        named.insert(copy_string(name)?, slot);
    }
    // Lower the body ONCE, with every union name open, and wrap it in the
    // shared `ChainBody` barrier every arm's frame ends at.
    let mut opened_names = Vec::new();
    for (name, slot) in &named {
        lowerer.scopes.open(name, *slot)?;
        opened_names.push(name.clone());
    }
    let body = lower_expr(&binding.body, source, lowerer);
    for _ in &opened_names {
        lowerer.scopes.pop();
    }
    let body = body?;
    let shared_body = push_node(&mut lowerer.nodes, ProgramNode::ChainBody { body })?;
    let mut arms = Vec::new();
    for alternative in &alternatives {
        let (frame, opened) = open_alternative_arm(alternative, matched, &union, &named, source, lowerer)?;
        close_pattern_scope(opened, lowerer);
        let arm = wrap_pattern_frame(shared_body, frame, &mut lowerer.nodes)?;
        arms.try_reserve(1)
            .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
        arms.push(arm);
    }
    let chain = chain_alternatives(&arms, dot, &mut lowerer.nodes)?;
    let matched_frame = push_node(
        &mut lowerer.nodes,
        ProgramNode::Bind {
            source: source_id,
            slot: matched,
            body: chain,
            frame: false,
        },
    )?;
    push_node(
        &mut lowerer.nodes,
        ProgramNode::Bind {
            source: incoming,
            slot: dot,
            body: matched_frame,
            frame: false,
        },
    )
}

/// Whether `name` is a resident `~` constructor: the closed list the
/// constructor dispatch and the value-position messages share.
fn is_engine_constructor(name: &str) -> bool {
    matches!(name, "generator" | "cursor" | "inputs" | "rng")
}

/// The value-position rejection reason for a constructor reference: the
/// constructor must be the value of an `as ~x` binder, or its cursor could
/// never be pulled.
fn constructor_must_bind(name: &str) -> alloc::string::String {
    alloc::format!(
        "an engine constructor (`~{name}(...)`) must bind to an engine binding \
                    (`as ~x | ...`)"
    )
}

/// Lowers the engine binding form `~CONSTRUCTOR(...) as ~x | BODY` — the `~`
/// namespace's way to introduce a cursor (`~generator`, `~cursor`).
///
/// The constructor's arguments lower in the CURRENT engine scope (outer
/// bindings are visible; a pull of one is rejected as cross-machine capture),
/// the constructor's graph becomes the cursor's seed (an `EngineGenerator`
/// node for `~generator`'s phase drive, the argument graph itself for
/// `~cursor`), a fresh engine slot is allocated for the `~x` occurrence, and
/// the body lowers with `~x` open. The resulting [`ProgramNode::EngineBind`]
/// evaluates the cursor ONCE, runs the body once over the original input, and
/// releases the cursor at body end (RAII).
fn lower_engine_binding<'ast>(
    binding: &'ast jqf_syntax::BindingExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let value = &binding.value;
    // The seed graph the cursor is built from. `~CONSTRUCTOR(...)` is the
    // ordinary path; the bare `~inputs` term is the input-sequence cursor
    // resident, lowered onto the SAME machinery as `~cursor(inputs)` (one
    // cursor engine) with a resident marker that scopes it to the null-first
    // drive (a cursor over the input sequence collides with the per-element
    // cursor-store reset, so the resident is a `-n`-only contract enforced at
    // route planning).
    let seed = match value.kind() {
        ExprKind::EngineCall { call, .. } => {
            let constructor = source.text().get(call.name.range()).ok_or_else(|| {
                EngineCompileError::Parse(ParseRejection::internal("engine constructor name span out of range"))
            })?;
            lower_engine_constructor(constructor, call, value, source, lowerer)?
        }
        // `~inputs`: the resident input cursor. Lowered as `~cursor(inputs)`
        // would be — the shared input sequence pulled one value per cursor
        // pull — and marked so the CLI can scope it to the null-first drive.
        ExprKind::EngineTerm { name, .. } => {
            let name_text = source.text().get(name.range()).ok_or_else(|| {
                EngineCompileError::Parse(ParseRejection::internal("engine term name span out of range"))
            })?;
            if name_text != "inputs" {
                return Err(EngineCompileError::UndefinedEngineConstructor {
                    span: value.span(),
                    name: try_copy_str(name_text).unwrap_or_else(|| String::from("<constructor>")),
                });
            }
            lowerer.uses_inputs_cursor = true;
            builtin_call("inputs", &[], lowerer)?
        }
        _ => {
            return Err(EngineCompileError::EngineBindingShape {
                span: value.span(),
                reason: alloc::string::String::from(
                    "an engine binding (`~x`) must bind an engine constructor \
                     (`~generator(...)`)",
                ),
            });
        }
    };
    let pattern_text = source.text().get(binding.pattern.span().range()).ok_or_else(|| {
        EngineCompileError::Parse(ParseRejection::internal("engine binding pattern span out of range"))
    })?;
    let name = pattern_text.strip_prefix('~').unwrap_or(pattern_text);
    let slot = lowerer.engine_scopes.push(name)?;
    let body = lower_expr(&binding.body, source, lowerer);
    lowerer.engine_scopes.pop();
    let body = body?;
    push_node(
        &mut lowerer.nodes,
        ProgramNode::EngineBind {
            source: seed,
            slot,
            body,
        },
    )
}

/// Lowers one engine-constructor CALL into its cursor seed graph — the
/// resident dispatch behind [`lower_engine_binding`]'s `EngineCall` arm.
/// `~generator` builds a phase-driven state machine; `~cursor` wraps an
/// arbitrary generator graph directly (the `EngineBind` source IS that graph
/// — one cursor engine, one pull protocol); `~rng` arms a xoshiro256** from
/// its seed.
fn lower_engine_constructor<'ast>(
    constructor: &str,
    call: &'ast jqf_syntax::CallExpr,
    value: &'ast Expr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    match constructor {
        "generator" => {
            if call.args.len() != 3 {
                return Err(EngineCompileError::EngineBindingShape {
                    span: value.span(),
                    reason: alloc::string::String::from(
                        "`~generator` expects exactly three arguments (init; update; extract)",
                    ),
                });
            }
            lowerer.in_engine_constructor += 1;
            let init = lower_expr(&call.args[0].expression, source, lowerer);
            let update = lower_expr(&call.args[1].expression, source, lowerer);
            let extract = lower_expr(&call.args[2].expression, source, lowerer);
            lowerer.in_engine_constructor -= 1;
            let (init, update, extract) = (init?, update?, extract?);
            push_node(
                &mut lowerer.nodes,
                ProgramNode::EngineGenerator { init, update, extract },
            )
        }
        "cursor" => {
            // `~cursor(f)`: the cursor's body is the argument graph itself.
            // The cursor machine seeds at f and routes f's outputs as pulls —
            // no phase drive, so `~generator`'s cardinality law does not apply
            // to a cursor (a multi-output f is a multi-value pull, exactly as
            // a `.[]` over a multi-member container would be).
            let [argument] = call.args.as_slice() else {
                return Err(EngineCompileError::EngineBindingShape {
                    span: value.span(),
                    reason: alloc::string::String::from("`~cursor` expects exactly one argument (the generator graph)"),
                });
            };
            lowerer.in_engine_constructor += 1;
            let body = lower_expr(&argument.expression, source, lowerer);
            lowerer.in_engine_constructor -= 1;
            body
        }
        "rng" => {
            // `~rng($seed)`: the seed graph's one exact-integer output arms a
            // xoshiro256** whose draws share `rand(seed)`'s law. The seed is
            // evaluated ONCE at bind over the bind-site dot (`~rng(.)` seeds
            // from it), exactly like `~generator`'s init.
            let [argument] = call.args.as_slice() else {
                return Err(EngineCompileError::EngineBindingShape {
                    span: value.span(),
                    reason: alloc::string::String::from("`~rng` expects exactly one argument (the integer seed)"),
                });
            };
            lowerer.in_engine_constructor += 1;
            let seed = lower_expr(&argument.expression, source, lowerer);
            lowerer.in_engine_constructor -= 1;
            push_node(&mut lowerer.nodes, ProgramNode::EngineRng { seed: seed? })
        }
        _ => Err(EngineCompileError::UndefinedEngineConstructor {
            span: value.span(),
            name: try_copy_str(constructor).unwrap_or_else(|| String::from("<constructor>")),
        }),
    }
}

/// The variable-name union of a `?//` chain, in source order.
fn alternative_union(alternatives: &[&Pattern], source: &SyntaxSource<'_>) -> Result<Vec<String>, EngineCompileError> {
    let mut union = Vec::new();
    for alternative in alternatives {
        collect_pattern_names(alternative, source, &mut union)?;
    }
    Ok(union)
}

/// Lowers `reduce`/`foreach SOURCE as $x (INIT; UPDATE[; EXTRACT])`.
///
/// Scope discipline: the loop SOURCE and the INIT are outside the binding (the
/// init sees the outer dot, not `$x`), while UPDATE and the
/// 3-arg EXTRACT are inside it. A `reduce` with an extract, and a `foreach`
/// without an update, are parser-level shapes this lowering rejects by name.
///
/// A DESTRUCTURING binding puts the pattern frame in the loop's SOURCE, so the
/// stream the fold walks is one value per BINDING SET rather than one per source
/// value.
///
/// That placement is what makes the state thread correctly: the fold binds the
/// matcher around a block holding the state load, the update, the state store
/// AND the extract, so a pattern that yields two binding sets for one source
/// value performs two full fold steps:
/// `[{"a":1,"b":2}] | [foreach .[] as {("a","b"):$x} (0; .+$x)]` is `[1,3]`, not
/// `[1,2]`. Wrapping the UPDATE instead makes the two sets siblings of ONE step,
/// which reads the same incoming state twice — the wrong answer, and the reason
/// this is not the obvious shape.
///
/// The update and the extract then read the frame's slots without re-deriving
/// them: they run on each source emission, with that emission's binder frame
/// still live, and a slot is never restored (see
/// [`Scopes::allocate_anonymous`]). Re-deriving would re-enumerate a generator
/// key and multiply the outputs.
fn lower_loop<'ast>(
    loop_expr: &'ast LoopExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
    is_foreach: bool,
) -> Result<ProgramNodeId, EngineCompileError> {
    if !is_foreach && loop_expr.extract.is_some() {
        return Err(EngineCompileError::unsupported(
            loop_expr.keyword_span,
            UnsupportedConstruct::Expression("a three-argument `reduce`"),
        ));
    }
    // Loops bind VALUES; only `as ~x` introduces a cursor. An engine binding
    // in a loop pattern is rejected at lower time.
    if matches!(loop_expr.binding.kind(), PatternKind::EngineBinding) {
        return Err(EngineCompileError::EngineBindingLoopPattern {
            span: loop_expr.binding.span(),
        });
    }
    let source_id = lower_expr(&loop_expr.source, source, lowerer)?;
    let init = lower_expr(&loop_expr.init, source, lowerer)?;
    let mut alternatives = Vec::new();
    flatten_alternatives(&loop_expr.binding, &mut alternatives)?;
    if alternatives.len() > 1 {
        return lower_alternative_loop(loop_expr, &alternatives, source_id, init, source, lowerer, is_foreach);
    }
    let mut frame = Vec::new();
    let (stream, slot, opened) = if matches!(loop_expr.binding.kind(), PatternKind::Variable) {
        let name = pattern_variable(&loop_expr.binding, source)?;
        (source_id, lowerer.scopes.push(name)?, 1)
    } else {
        let named = alloc::collections::BTreeMap::new();
        collect_pattern_bindings(&loop_expr.binding, source_id, source, lowerer, &mut frame, &named)?;
        let matched = frame.first().map(|binding| binding.slot).ok_or_else(|| {
            EngineCompileError::Parse(ParseRejection::internal("a destructuring pattern collected no binder"))
        })?;
        let emission = push_node(&mut lowerer.nodes, variable_stage(matched))?;
        let opened = open_pattern_scope(&frame, lowerer)?;
        let stream = wrap_pattern_frame(emission, frame, &mut lowerer.nodes)?;
        (stream, lowerer.scopes.allocate_anonymous()?, opened)
    };
    let scoped = lower_loop_body(loop_expr, source, lowerer);
    close_pattern_scope(opened, lowerer);
    let (update, extract) = scoped?;
    push_node(
        &mut lowerer.nodes,
        if is_foreach {
            ProgramNode::Foreach {
                source: stream,
                slot,
                init,
                update,
                extract,
            }
        } else {
            ProgramNode::Reduce {
                source: stream,
                slot,
                init,
                update,
                keyed_collect: None,
            }
        },
    )
}

/// Lowers a fold whose as-clause is a `?//` chain.
///
/// Here the barrier has to cover the UPDATE, not just the extraction: the next
/// alternative restarts when the update raises
/// (`[[3]] | [reduce .[] as [$a] ?// $b (0; if $a then error("x") else 99 end)]` is
/// `[99]`, alternative 2 binding `$b` and answering from `$a == null`). So the
/// chain lives INSIDE the update, over the fold's own binder as the matched value,
/// and each arm restores dot from the incoming ACCUMULATOR — the update's dot —
/// rather than from the program input.
///
/// A three-argument `foreach` is refused: the extract sits inside the same
/// barrier block, so an extract raise restarts too
/// (`[[3]] | [foreach .[] as [$a] ?// $b (0; .+1; if $a then error("x") else 99
/// end)]` is `[99]`), and [`ProgramNode::Foreach`] holds the update and the extract
/// as separate children that one `Try` cannot span. Landing the two-argument form
/// and naming the cut beats landing a barrier that silently stops at the update.
fn lower_alternative_loop<'ast>(
    loop_expr: &'ast LoopExpr,
    alternatives: &[&'ast Pattern],
    stream: ProgramNodeId,
    init: ProgramNodeId,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
    is_foreach: bool,
) -> Result<ProgramNodeId, EngineCompileError> {
    if loop_expr.extract.is_some() {
        return Err(EngineCompileError::unsupported(
            loop_expr.keyword_span,
            UnsupportedConstruct::Expression("a `?//` destructuring alternative in a three-argument `foreach`"),
        ));
    }
    let accumulator = push_node(&mut lowerer.nodes, current_stage())?;
    let carried = lowerer.scopes.allocate_anonymous()?;
    let matched = lowerer.scopes.allocate_anonymous()?;
    let union = alternative_union(alternatives, source)?;
    let mut named = alloc::collections::BTreeMap::new();
    for name in &union {
        let slot = lowerer.scopes.allocate_anonymous()?;
        named.insert(copy_string(name)?, slot);
    }
    let mut opened_names = Vec::new();
    for (name, slot) in &named {
        lowerer.scopes.open(name, *slot)?;
        opened_names.push(name.clone());
    }
    let update = lower_expr(&loop_expr.update, source, lowerer);
    for _ in &opened_names {
        lowerer.scopes.pop();
    }
    let update = update?;
    let shared_body = push_node(&mut lowerer.nodes, ProgramNode::ChainBody { body: update })?;
    let mut arms = Vec::new();
    for alternative in alternatives {
        let (frame, opened) = open_alternative_arm(alternative, matched, &union, &named, source, lowerer)?;
        close_pattern_scope(opened, lowerer);
        let arm = wrap_pattern_frame(shared_body, frame, &mut lowerer.nodes)?;
        arms.try_reserve(1)
            .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
        arms.push(arm);
    }
    let chain = chain_alternatives(&arms, carried, &mut lowerer.nodes)?;
    let update = push_node(
        &mut lowerer.nodes,
        ProgramNode::Bind {
            source: accumulator,
            slot: carried,
            body: chain,
            frame: false,
        },
    )?;
    push_node(
        &mut lowerer.nodes,
        if is_foreach {
            ProgramNode::Foreach {
                source: stream,
                slot: matched,
                init,
                update,
                extract: None,
            }
        } else {
            ProgramNode::Reduce {
                source: stream,
                slot: matched,
                init,
                update,
                keyed_collect: None,
            }
        },
    )
}

/// Lowers the parts of a loop that sit INSIDE the binding scope: the update and
/// the optional 3-arg extract.
fn lower_loop_body<'ast>(
    loop_expr: &'ast LoopExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<(ProgramNodeId, Option<ProgramNodeId>), EngineCompileError> {
    let update = lower_expr(&loop_expr.update, source, lowerer)?;
    let extract = match &loop_expr.extract {
        Some(extract) => Some(lower_expr(extract, source, lowerer)?),
        None => None,
    };
    Ok((update, extract))
}

/// Lowers one builtin call, resolving `(name, arity)` before lowering arguments.
///
/// Resolution runs against the registry first (AGENTS.md). An unresolved
/// `(name, arity)` is the `name/arity is not defined`. A resolved `Evaluator`
/// (`length`, `keys`, `select`) becomes a [`ProgramNode::Call`] storing the
/// stable overload id and its separate semantic revision; a resolved `Lowering`
/// (`map`) expands into the arena instead of ever becoming a `Call`.
fn lower_call<'ast>(
    call: &'ast CallExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let name = source
        .text()
        .get(call.name.range())
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("call name span out of range")))?;
    // A visible `def` — or a FILTER PARAMETER of the definition being inlined —
    // wins over a builtin of the same `(name, arity)`. That is the rule: a user
    // `def length: 1;` shadows the builtin for the rest of its scope.
    if let Some(inlined) = lower_user_call(call, name, source, lowerer) {
        return inlined;
    }
    // The registry keys an overload by a one-byte arity, so a call with more
    // than 255 arguments can never resolve. Rejecting here names the AUTHORED
    // count; clamping into `u8` would report `name/255 is not defined`, an
    // arity the user never wrote.
    let Ok(arity) = u8::try_from(call.args.len()) else {
        return Err(EngineCompileError::arity_limit(
            call.name,
            name,
            u32::try_from(call.args.len()).unwrap_or(u32::MAX),
        ));
    };
    let Some(record) = resolve_builtin(name, arity) else {
        return Err(EngineCompileError::undefined_call(call.name, name, arity));
    };
    match record.execution {
        // `map` expands at lower time; every other resolved Lowering is unknown.
        BuiltinExecution::Lowering => match dispatch(record.id) {
            Some(BuiltinDispatch::Lowering(Lowering::Map)) => lower_map(call, source, lowerer),
            Some(BuiltinDispatch::Lowering(Lowering::First)) => lower_first(call, source, lowerer),
            Some(BuiltinDispatch::Lowering(Lowering::Limit)) => lower_limit(call, source, lowerer),
            Some(BuiltinDispatch::Lowering(Lowering::NthIndex)) => lower_nth_index(call, source, lowerer),
            Some(BuiltinDispatch::Lowering(Lowering::Nth)) => lower_nth(call, source, lowerer),
            Some(BuiltinDispatch::Lowering(Lowering::Skip)) => lower_skip(call, source, lowerer),
            Some(BuiltinDispatch::Lowering(Lowering::PathsFiltered)) => lower_paths(Some(call), source, lowerer),
            Some(BuiltinDispatch::Lowering(Lowering::Del)) => lower_del(call, source, lowerer),
            Some(BuiltinDispatch::Lowering(Lowering::WithEntries)) => lower_with_entries(call, source, lowerer),
            Some(BuiltinDispatch::Lowering(Lowering::Add)) => lower_add(call, source, lowerer),
            Some(BuiltinDispatch::Lowering(Lowering::In)) => lower_in(call, source, lowerer),
            Some(BuiltinDispatch::Lowering(Lowering::Pick)) => lower_pick(call, source, lowerer),
            Some(BuiltinDispatch::Lowering(Lowering::InStream)) => lower_in_stream(call, source, lowerer),
            Some(BuiltinDispatch::Lowering(Lowering::Index)) => lower_indexed_fold(call, source, lowerer),
            Some(BuiltinDispatch::Lowering(Lowering::JoinIndexed)) => lower_join_indexed(call, source, lowerer),
            Some(BuiltinDispatch::Lowering(Lowering::Inside)) => lower_inside(call, source, lowerer),
            _ => Err(EngineCompileError::Parse(ParseRejection::internal(
                "resolved lowering has no expansion",
            ))),
        },
        BuiltinExecution::Evaluator => {
            let mut args = Vec::new();
            args.try_reserve(call.args.len())
                .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
            for argument in &call.args {
                args.push(lower_expr(&argument.expression, source, lowerer)?);
            }
            push_node(
                &mut lowerer.nodes,
                ProgramNode::call(record.id, record.semantic_revision, args),
            )
        }
        // No `Definition`/`Operator` overload is registered; a resolved one
        // reaching here is a registry/compiler contract violation.
        BuiltinExecution::Definition | BuiltinExecution::Operator => Err(EngineCompileError::Parse(
            ParseRejection::internal("resolved builtin has an unsupported execution kind"),
        )),
    }
}

/// Expands `map(f)` into the `[.[] | f]` arena graph (the Lowering contract):
/// `CollectArray(FlatMap(Stage[Each], f))`. Ordinary fusion
/// then applies, so `map(f)` and `[.[] | f]` become the identical plan.
fn lower_map<'ast>(
    call: &'ast CallExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let argument = call
        .args
        .first()
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("map/1 resolved with no argument")))?;
    // The argument lowers through the ordinary path, so the enclosing lexical
    // scopes thread into a call body: `map(reduce .[] as $x (0; .+$x))` works,
    // and so does a `$x` from an outer binder inside the mapped filter.
    let body = lower_expr(&argument.expression, source, lowerer)?;
    collect_each(body, lowerer)
}

/// Lowers `paths` and `paths(f)` into their definitions:
///
/// ```text
/// def paths:    path(..)              | select(length > 0);
/// def paths(f): path(.. | select(f))  | select(length > 0);
/// ```
///
/// The `length > 0` filter is what EXCLUDES THE ROOT, and it is the whole reason
/// a scalar input produces nothing: `..` always emits the root at the empty path,
/// and the empty path has length zero.
fn lower_paths<'ast>(
    call: Option<&'ast CallExpr>,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let descend = push_node(
        &mut lowerer.nodes,
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: descend_steps()?,
        },
    )?;
    let enumerated = match call {
        None => descend,
        Some(call) => {
            let argument = call.args.first().ok_or_else(|| {
                EngineCompileError::Parse(ParseRejection::internal("paths/1 resolved with no argument"))
            })?;
            let predicate = lower_expr(&argument.expression, source, lowerer)?;
            let body = builtin_call("select", &[predicate], lowerer)?;
            push_node(
                &mut lowerer.nodes,
                ProgramNode::FlatMap {
                    upstream: descend,
                    body,
                },
            )?
        }
    };
    let located = builtin_call("path", &[enumerated], lowerer)?;
    let length = builtin_call("length", &[], lowerer)?;
    let zero = push_node(&mut lowerer.nodes, integer_stage(0))?;
    let non_empty = push_node(
        &mut lowerer.nodes,
        ProgramNode::Binary {
            op: BinaryKind::Greater,
            left: length,
            right: zero,
            shape: crate::program::BinaryShape::Framed,
        },
    )?;
    let keep = builtin_call("select", &[non_empty], lowerer)?;
    push_node(
        &mut lowerer.nodes,
        ProgramNode::FlatMap {
            upstream: located,
            body: keep,
        },
    )
}

/// Lowers `del(f)` into its definition: `delpaths([path(f)])`.
///
/// The collection is load-bearing, not incidental: `delpaths` deletes its whole
/// path SET simultaneously, so `del(.[0], .[1])` removes both ORIGINAL positions
/// rather than deleting one and re-addressing the other.
fn lower_del<'ast>(
    call: &'ast CallExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let argument = call
        .args
        .first()
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("del/1 resolved with no argument")))?;
    let body = lower_expr(&argument.expression, source, lowerer)?;
    let located = builtin_call("path", &[body], lowerer)?;
    let collected = push_node(&mut lowerer.nodes, ProgramNode::CollectArray { body: Some(located) })?;
    builtin_call("delpaths", &[collected], lowerer)
}

/// Lowers `with_entries(f)` into its definition:
/// `to_entries | map(f) | from_entries`.
///
/// Every one of `with_entries`' surprises is a consequence of that pipeline and
/// none of them needs code here: `map` collects ALL of `f`'s outputs, so
/// `with_entries(.value+=1, .value+=2)` builds ONE object out of four entries;
/// a `select` that drops an entry drops the key; and `from_entries`' last-wins
/// duplicate law decides what a renamed key collides with.
fn lower_with_entries<'ast>(
    call: &'ast CallExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let argument = call.args.first().ok_or_else(|| {
        EngineCompileError::Parse(ParseRejection::internal("with_entries/1 resolved with no argument"))
    })?;
    let body = lower_expr(&argument.expression, source, lowerer)?;
    let entries = builtin_call("to_entries", &[], lowerer)?;
    let mapped = collect_each(body, lowerer)?;
    let piped = push_node(
        &mut lowerer.nodes,
        ProgramNode::FlatMap {
            upstream: entries,
            body: mapped,
        },
    )?;
    let rebuild = builtin_call("from_entries", &[], lowerer)?;
    push_node(
        &mut lowerer.nodes,
        ProgramNode::FlatMap {
            upstream: piped,
            body: rebuild,
        },
    )
}

/// Lowers `add(f)` into its definition:
///
/// ```text
/// def add(f): reduce f as $x (null; . + $x);
/// ```
///
/// Every edge case is `+`'s and `reduce`'s, not this function's: the `null`
/// seed is why an empty source answers `null` rather than `0`, and it is also
/// why `add` over strings, arrays or objects works — `null` is `+`'s identity
/// for every one of them. `add/0` is the SAME definition with `.[]` as the
/// source, but it dispatches to a native evaluator
/// ([`jqf_builtins::registry::builtins::reshape::add`]) rather than through here: a
/// fixed `.[]` source folds in a loop, while `add(f)`'s arbitrary filter
/// source only the machine can drive.
fn lower_add<'ast>(
    call: &'ast CallExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let [argument] = call.args.as_slice() else {
        return Err(EngineCompileError::Parse(ParseRejection::internal(
            "add/1 resolved without exactly one argument",
        )));
    };
    let stream = lower_expr(&argument.expression, source, lowerer)?;
    let item = lowerer.scopes.allocate_anonymous()?;
    let init = push_node(&mut lowerer.nodes, literal_stage(Value::Null))?;
    let accumulator = push_node(&mut lowerer.nodes, current_stage())?;
    let addend = push_node(&mut lowerer.nodes, variable_stage(item))?;
    let update = push_node(
        &mut lowerer.nodes,
        ProgramNode::Binary {
            op: BinaryKind::Add,
            left: accumulator,
            right: addend,
            shape: crate::program::BinaryShape::Framed,
        },
    )?;
    push_node(
        &mut lowerer.nodes,
        ProgramNode::Reduce {
            source: stream,
            slot: item,
            init,
            update,
            keyed_collect: None,
        },
    )
}

/// Lowers `in(xs)` into its definition: `. as $x | xs | has($x)`.
///
/// The binding is what swaps the operands, and expanding rather than writing a
/// second evaluator is what keeps `in` and `has` from ever disagreeing: a
/// multi-output `xs` answers once per container, and every refusal is `has`'s
/// own with the kinds in `has`'s order.
fn lower_in<'ast>(
    call: &'ast CallExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let argument = call
        .args
        .first()
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("in/1 resolved with no argument")))?;
    let key = lowerer.scopes.allocate_anonymous()?;
    let containers = lower_expr(&argument.expression, source, lowerer)?;
    let bound = push_node(&mut lowerer.nodes, variable_stage(key))?;
    let question = builtin_call("has", &[bound], lowerer)?;
    let body = push_node(
        &mut lowerer.nodes,
        ProgramNode::FlatMap {
            upstream: containers,
            body: question,
        },
    )?;
    let current = push_node(&mut lowerer.nodes, current_stage())?;
    push_node(
        &mut lowerer.nodes,
        ProgramNode::Bind {
            source: current,
            slot: key,
            body,
            frame: false,
        },
    )
}

/// Lowers `pick(pathexps)` into its definition:
///
/// ```text
/// def pick(pathexps):
///     . as $top | reduce path(pathexps) as $p (null; setpath($p; $top | getpath($p)));
/// ```
///
/// The seed is `null` and NOT the input, which is the whole builtin: the result
/// is a fresh skeleton grown by `setpath`, so `pick(empty)` answers `null`, an
/// array path grows an array padded with nulls, and a sibling the paths never
/// name simply never appears. Reading through `$top` rather than through the
/// accumulator is what makes several paths independent of each other.
fn lower_pick<'ast>(
    call: &'ast CallExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let argument = call
        .args
        .first()
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("pick/1 resolved with no argument")))?;
    let top = lowerer.scopes.allocate_anonymous()?;
    let expressions = lower_expr(&argument.expression, source, lowerer)?;
    let locations = builtin_call("path", &[expressions], lowerer)?;
    let step = lowerer.scopes.allocate_anonymous()?;

    let init = push_node(&mut lowerer.nodes, literal_stage(Value::Null))?;
    let read_at = push_node(&mut lowerer.nodes, variable_stage(step))?;
    let read = builtin_call("getpath", &[read_at], lowerer)?;
    let original = push_node(&mut lowerer.nodes, variable_stage(top))?;
    let value = push_node(
        &mut lowerer.nodes,
        ProgramNode::FlatMap {
            upstream: original,
            body: read,
        },
    )?;
    let write_at = push_node(&mut lowerer.nodes, variable_stage(step))?;
    let update = builtin_call("setpath", &[write_at, value], lowerer)?;
    let fold = push_node(
        &mut lowerer.nodes,
        ProgramNode::Reduce {
            source: locations,
            slot: step,
            init,
            update,
            keyed_collect: None,
        },
    )?;
    let current = push_node(&mut lowerer.nodes, current_stage())?;
    push_node(
        &mut lowerer.nodes,
        ProgramNode::Bind {
            source: current,
            slot: top,
            body: fold,
            frame: false,
        },
    )
}

/// Lowers `IN(s)` and `IN(s; t)` into their definitions:
///
/// ```text
/// def IN(s):   any(s == .; .);
/// def IN(s; t): any(s == t; .);
/// ```
///
/// The FIRST argument is the value searched for and the SECOND the stream
/// searched in, exactly as the definition spells it: `s` (the first argument)
/// is the LEFT
/// operand of `==`, `t` (the second) the RIGHT.
///
/// `any` is not reachable as a call here (it lives in the stdlib prelude, which
/// this registry does not call into), so the SHAPE `any` compiles to is written
/// out: `label $out | ((s == subject | if . then (true, break $out) else empty
/// end), false)`.
///
/// The `break` is the short circuit and it is observable, not an optimization:
/// `1 | IN(1, error("x"))` is `true` because the exit fires before the second
/// output is ever demanded. The trailing `false` is reached only when
/// the comparison stream runs out, which is why `IN(empty)` is `false` and not
/// empty. The exit target is ANONYMOUS, so a `break` to a user label inside `s`
/// still reaches that user label.
fn lower_in_stream<'ast>(
    call: &'ast CallExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let (subject_arg, set_arg) = match call.args.as_slice() {
        [set] => (None, set),
        [subject, set] => (Some(subject), set),
        _ => {
            return Err(EngineCompileError::Parse(ParseRejection::internal(
                "IN resolved without one or two arguments",
            )));
        }
    };
    // The operand order: `any(s == t; .)` evaluates the RIGHT argument of `==`
    // (the outer loop) first, so the second IN argument is the outer loop and
    // the subject the inner. The order is observable in the empty-cancellation
    // law: `IN(error; empty)` answers `false` because the empty stream is
    // evaluated before `error` is ever reached.
    let candidates = lower_expr(&set_arg.expression, source, lowerer)?;
    let subject = match subject_arg {
        Some(argument) => lower_expr(&argument.expression, source, lowerer)?,
        None => push_node(&mut lowerer.nodes, current_stage())?,
    };
    let comparison = push_node(
        &mut lowerer.nodes,
        ProgramNode::Binary {
            op: BinaryKind::Equal,
            left: subject,
            right: candidates,
            shape: crate::program::BinaryShape::Framed,
        },
    )?;
    let slot = lowerer.labels.allocate_anonymous()?;
    let hit = push_node(&mut lowerer.nodes, literal_stage(Value::Bool(true)))?;
    let exit = push_node(&mut lowerer.nodes, ProgramNode::Break { slot })?;
    let answer = push_node(&mut lowerer.nodes, ProgramNode::Choice { left: hit, right: exit })?;
    let miss = push_node(&mut lowerer.nodes, ProgramNode::Empty)?;
    let matched = push_node(&mut lowerer.nodes, current_stage())?;
    let gate = push_node(
        &mut lowerer.nodes,
        ProgramNode::Conditional {
            condition: matched,
            consequent: answer,
            alternative: miss,
        },
    )?;
    let scan = push_node(
        &mut lowerer.nodes,
        ProgramNode::FlatMap {
            upstream: comparison,
            body: gate,
        },
    )?;
    let exhausted = push_node(&mut lowerer.nodes, literal_stage(Value::Bool(false)))?;
    let body = push_node(
        &mut lowerer.nodes,
        ProgramNode::Choice {
            left: scan,
            right: exhausted,
        },
    )?;
    push_node(&mut lowerer.nodes, ProgramNode::Label { slot, body })
}

/// Lowers `INDEX(idx_expr)` and `INDEX(stream; idx_expr)` into their
/// definition, with the key REBOUND:
///
/// ```text
/// def INDEX(stream; idx_expr): reduce stream as $row ({}; .[$row|idx_expr|tostring] = $row);
/// def INDEX(idx_expr):         INDEX(.[]; idx_expr);
///
/// jqf:  reduce stream as $row ({}; reduce ($row|idx_expr|tostring) as $k (.; .[$k] = $row))
/// ```
///
/// The reference indexes by an arbitrary expression where jqf indexes by a
/// variable slot, so the key is bound first. The INNER REDUCE — rather than a
/// plain `as $k | .[$k] = $row` — is what preserves the cardinality law: `=`
/// is ONE assignment over however many paths its left side names, so
/// `INDEX(.a, .b)` files the row under BOTH keys in one object and
/// `INDEX(empty)` files it under none while leaving the accumulator intact.
/// Folding the key stream from the accumulator reproduces both; a binding would
/// publish one object per key (losing all but the last) and nothing at all for
/// `empty` (nulling the accumulator).
fn lower_indexed_fold<'ast>(
    call: &'ast CallExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let (stream_arg, key_arg) = match call.args.as_slice() {
        [key] => (None, key),
        [stream, key] => (Some(stream), key),
        _ => {
            return Err(EngineCompileError::Parse(ParseRejection::internal(
                "INDEX resolved without one or two arguments",
            )));
        }
    };
    let stream = match stream_arg {
        Some(argument) => lower_expr(&argument.expression, source, lowerer)?,
        None => push_node(
            &mut lowerer.nodes,
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: each_steps()?,
            },
        )?,
    };
    let row = lowerer.scopes.allocate_anonymous()?;

    // `$row | idx_expr | tostring`
    let key_filter = lower_expr(&key_arg.expression, source, lowerer)?;
    let subject = push_node(&mut lowerer.nodes, variable_stage(row))?;
    let keyed = push_node(
        &mut lowerer.nodes,
        ProgramNode::FlatMap {
            upstream: subject,
            body: key_filter,
        },
    )?;
    let rendered = builtin_call("tostring", &[], lowerer)?;
    let keys = push_node(
        &mut lowerer.nodes,
        ProgramNode::FlatMap {
            upstream: keyed,
            body: rendered,
        },
    )?;

    // `reduce <keys> as $k (.; .[$k] = $row)`
    let key = lowerer.scopes.allocate_anonymous()?;
    let accumulator = push_node(&mut lowerer.nodes, current_stage())?;
    let mut steps = Vec::new();
    steps
        .try_reserve_exact(1)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    steps.push(StageStep::new(StepAccess::DynVar(key), false));
    let target = push_node(
        &mut lowerer.nodes,
        ProgramNode::Stage {
            start: StageStart::Current,
            steps,
        },
    )?;
    let stored = push_node(&mut lowerer.nodes, variable_stage(row))?;
    let write = modify_node(target, stored, ModifyMode::Set, lowerer)?;
    let file = push_node(
        &mut lowerer.nodes,
        ProgramNode::Reduce {
            source: keys,
            slot: key,
            init: accumulator,
            update: write,
            keyed_collect: None,
        },
    )?;

    let empty = empty_object_stage(lowerer)?;
    push_node(
        &mut lowerer.nodes,
        ProgramNode::Reduce {
            source: stream,
            slot: row,
            init: empty,
            update: file,
            keyed_collect: None,
        },
    )
}

/// Lowers the three `JOIN` arities into their definitions, with the lookup
/// key REBOUND:
///
/// ```text
/// def JOIN($idx; idx_expr):                    [.[] | [., $idx[idx_expr]]];
/// def JOIN($idx; stream; idx_expr):            stream | [., $idx[idx_expr]];
/// def JOIN($idx; stream; idx_expr; join_expr): stream | [., $idx[idx_expr]] | join_expr;
///
/// jqf:  $idx[idx_expr]  ->  (idx_expr) as $k | $idx | .[$k]
/// ```
///
/// `$idx` is a VALUE parameter, so a multi-output index argument re-runs the
/// whole join once per index. The pair is built by a collector and not by a
/// constructor, which is why a multi-output key expression WIDENS the pair
/// (`[element, first, second]`) rather than producing two pairs, and why a
/// missing key contributes `null` — this is a left join, and no element is ever
/// dropped.
fn lower_join_indexed<'ast>(
    call: &'ast CallExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let (stream_arg, key_arg, join_arg) = match call.args.as_slice() {
        [_, key] => (None, key, None),
        [_, stream, key] => (Some(stream), key, None),
        [_, stream, key, join] => (Some(stream), key, Some(join)),
        _ => {
            return Err(EngineCompileError::Parse(ParseRejection::internal(
                "JOIN resolved without two, three or four arguments",
            )));
        }
    };
    let index_arg = call
        .args
        .first()
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("JOIN resolved with no arguments")))?;
    let index_source = lower_expr(&index_arg.expression, source, lowerer)?;
    let index = lowerer.scopes.allocate_anonymous()?;

    let stream = match stream_arg {
        Some(argument) => lower_expr(&argument.expression, source, lowerer)?,
        None => push_node(
            &mut lowerer.nodes,
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: each_steps()?,
            },
        )?,
    };

    // `(idx_expr) as $k | $idx | .[$k]`
    let key_filter = lower_expr(&key_arg.expression, source, lowerer)?;
    let key = lowerer.scopes.allocate_anonymous()?;
    let mut steps = Vec::new();
    steps
        .try_reserve_exact(1)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    steps.push(StageStep::new(StepAccess::DynVar(key), false));
    let read = push_node(
        &mut lowerer.nodes,
        ProgramNode::Stage {
            start: StageStart::Variable(index),
            steps,
        },
    )?;
    let lookup = push_node(
        &mut lowerer.nodes,
        ProgramNode::Bind {
            source: key_filter,
            slot: key,
            body: read,
            frame: false,
        },
    )?;

    // `[., <lookup>]`
    let element = push_node(&mut lowerer.nodes, current_stage())?;
    let members = push_node(
        &mut lowerer.nodes,
        ProgramNode::Choice {
            left: element,
            right: lookup,
        },
    )?;
    let pair = push_node(&mut lowerer.nodes, ProgramNode::CollectArray { body: Some(members) })?;
    let paired = push_node(
        &mut lowerer.nodes,
        ProgramNode::FlatMap {
            upstream: stream,
            body: pair,
        },
    )?;
    let body = match (stream_arg, join_arg) {
        // The arity-2 form is the only one that COLLECTS: it has no stream
        // argument of its own, so `.[]` is its stream and the pairs come back as
        // one array.
        (None, _) => push_node(&mut lowerer.nodes, ProgramNode::CollectArray { body: Some(paired) })?,
        (Some(_), None) => paired,
        (Some(_), Some(argument)) => {
            let finish = lower_expr(&argument.expression, source, lowerer)?;
            push_node(
                &mut lowerer.nodes,
                ProgramNode::FlatMap {
                    upstream: paired,
                    body: finish,
                },
            )?
        }
    };
    push_node(
        &mut lowerer.nodes,
        ProgramNode::Bind {
            source: index_source,
            slot: index,
            body,
            frame: false,
        },
    )
}

/// An empty-object literal stage (`{}`) — `INDEX`'s fold seed.
fn empty_object_stage(lowerer: &mut Lowerer<'_, '_>) -> Result<ProgramNodeId, EngineCompileError> {
    let object = jqf_data::ObjectBuilder::try_with_capacity(0)
        .and_then(jqf_data::ObjectBuilder::try_finish)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    push_node(&mut lowerer.nodes, literal_stage(Value::Object(object)))
}

/// `[.[] | body]` — the array-collecting fan-out `map` and `with_entries` share.
fn collect_each(body: ProgramNodeId, lowerer: &mut Lowerer<'_, '_>) -> Result<ProgramNodeId, EngineCompileError> {
    let each = push_node(
        &mut lowerer.nodes,
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: each_steps()?,
        },
    )?;
    let flatmap = push_node(&mut lowerer.nodes, ProgramNode::FlatMap { upstream: each, body })?;
    push_node(&mut lowerer.nodes, ProgramNode::CollectArray { body: Some(flatmap) })
}

/// A `Call` node for one registered EVALUATOR builtin, by name and arity.
fn builtin_call(
    name: &str,
    args: &[ProgramNodeId],
    lowerer: &mut Lowerer<'_, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let arity = u8::try_from(args.len()).unwrap_or(u8::MAX);
    let record = resolve_builtin(name, arity).ok_or_else(|| {
        EngineCompileError::Parse(ParseRejection::internal("a lowering names an unregistered builtin"))
    })?;
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(args.len())
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    owned.extend_from_slice(args);
    push_node(
        &mut lowerer.nodes,
        ProgramNode::call(record.id, record.semantic_revision, owned),
    )
}

/// Lowers `first(g)` into its definition:
/// `label $out | g | (., break $out)`.
///
/// Expanding rather than hand-writing an evaluator is deliberate. `first` is
/// DEFINED this way, so the expansion inherits every edge case exactly —
/// `first(empty)` emits nothing (the generator completes and the label pops),
/// an error raised before the first output propagates, and a `break` to a
/// USER label inside `g` still reaches that user label, because the expansion's
/// own exit target is anonymous ([`LabelScopes::allocate_anonymous`]).
fn lower_first<'ast>(
    call: &'ast CallExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let argument = call
        .args
        .first()
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("first/1 resolved with no argument")))?;
    // The generator lowers through the ordinary path, so enclosing scopes thread
    // into it exactly as they do for `map`'s body.
    let generator = lower_expr(&argument.expression, source, lowerer)?;
    cap_at_first_output(generator, lowerer)
}

/// Wraps one already-lowered generator in the first-output shape:
/// `label $out | GEN | (., break $out)`.
///
/// The exit target is ANONYMOUS ([`LabelScopes::allocate_anonymous`]), so a `break`
/// to a USER label inside the generator still reaches that user label. Two
/// callers share it — `first/1`, whose whole definition this is, and the
/// assignment lowering, whose UPDATE must stop at its first output rather than
/// merely ignore the rest (`.a |= (1, error("x"))` is `{"a":1}`, not an error,
/// because the break fires before the second output is ever demanded).
fn cap_at_first_output(
    generator: ProgramNodeId,
    lowerer: &mut Lowerer<'_, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let slot = lowerer.labels.allocate_anonymous()?;
    let identity = push_node(&mut lowerer.nodes, current_stage())?;
    let exit = push_node(&mut lowerer.nodes, ProgramNode::Break { slot })?;
    let emit_then_exit = push_node(
        &mut lowerer.nodes,
        ProgramNode::Choice {
            left: identity,
            right: exit,
        },
    )?;
    let body = push_node(
        &mut lowerer.nodes,
        ProgramNode::FlatMap {
            upstream: generator,
            body: emit_then_exit,
        },
    )?;
    push_node(&mut lowerer.nodes, ProgramNode::Label { slot, body })
}

/// Lowers all eight assignment operators into one [`ProgramNode::Modify`].
///
/// The table, which is the definitions read as one shape:
///
/// ```text
/// a  =  b   ->  b as $v | Modify{paths: a, update: $v,        mode: Set}
/// a |=  f   ->             Modify{paths: a, update: f,         mode: Update}
/// a op= b   ->  b as $v | Modify{paths: a, update: (. op $v),  mode: Update}
/// a //= b   ->  b as $v | Modify{paths: a, update: (. // $v),  mode: Update}
/// ```
///
/// **The bound right-hand side is the load-bearing part**, and it is what a
/// per-operator implementation gets wrong. Because `$v` is bound OUTSIDE the
/// fold: `.a += empty` emits nothing at all while `.a |= empty` emits the
/// document with `.a` deleted; `.a //= error("x")` RAISES even though `.a` is
/// truthy, since only the assignment is conditional and never the evaluation;
/// and a multi-output right-hand side is the OUTER loop, re-running the whole
/// fold once per value rather than fanning out per path.
///
/// `|=` is the one form with no binding: its right-hand side is a filter
/// evaluated at each target, which is exactly what `ModifyMode::Update` means.
fn lower_assignment<'ast>(
    assignment: &'ast AssignmentExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    // The accessor WRITE half: an assignment/update whose path contains a
    // node/attribute accessor step (`.@` / `.&`) lowers to a fact write ONLY in
    // the shapes the edit lane serves — the accessor as the target's LAST step
    // (a static selector naming an admitted role, or a dynamic selector bound
    // outside and validated at run time), over a resolvable key/index base
    // chain. Every other accessor write keeps the compile-time rejection, whose
    // wording names the served lane (dynamic selectors and computed bases
    // land; nested/dotted fact paths are rejected).
    if let Some(span) = find_accessor_step(&assignment.target) {
        let mut selector_binders = Vec::new();
        match fact_assign_target(&assignment.target, source, lowerer, &mut selector_binders)? {
            Some(target) => {
                return lower_fact_assignment(&target, &selector_binders, assignment, source, lowerer);
            }
            // Non-admitted static names and non-accessor last steps keep the
            // deferred-write rejection. A FACT target (`PATH.@comment = RHS`)
            // lowered above; it is not `--edit`-gated.
            None => {
                return Err(EngineCompileError::unsupported(
                    span,
                    UnsupportedConstruct::AccessorAssignment,
                ));
            }
        }
    }
    if assignment.op == AssignmentOp::Update {
        let paths = lower_expr(&assignment.target, source, lowerer)?;
        let update = lower_expr(&assignment.value, source, lowerer)?;
        return modify_node(paths, update, ModifyMode::Update, lowerer);
    }
    // `b as $v | …`: the value graph is lowered OUTSIDE the binding, exactly as
    // an authored `as` binding's source is.
    let bound_source = lower_expr(&assignment.value, source, lowerer)?;
    let slot = lowerer.scopes.allocate_anonymous()?;
    let paths = lower_expr(&assignment.target, source, lowerer)?;
    let bound = push_node(&mut lowerer.nodes, variable_stage(slot))?;
    let (update, mode) = match assignment.op {
        AssignmentOp::Assign => (bound, ModifyMode::Set),
        AssignmentOp::Alternative => {
            let current = push_node(&mut lowerer.nodes, current_stage())?;
            let update = push_node(
                &mut lowerer.nodes,
                ProgramNode::Alternative {
                    left: current,
                    right: bound,
                },
            )?;
            (update, ModifyMode::Update)
        }
        op => {
            let kind = arithmetic_update_kind(op).ok_or_else(|| {
                EngineCompileError::unsupported(
                    assignment.op_span,
                    UnsupportedConstruct::Expression("an unrecognized assignment operator"),
                )
            })?;
            let current = push_node(&mut lowerer.nodes, current_stage())?;
            let update = push_node(
                &mut lowerer.nodes,
                ProgramNode::Binary {
                    op: kind,
                    left: current,
                    right: bound,
                    shape: crate::program::BinaryShape::Framed,
                },
            )?;
            (update, ModifyMode::Update)
        }
    };
    let body = modify_node(paths, update, mode, lowerer)?;
    push_node(
        &mut lowerer.nodes,
        ProgramNode::Bind {
            source: bound_source,
            slot,
            body,
            frame: false,
        },
    )
}

/// The selector half of a fact-assignment target: the static role/kind text,
/// or the dynamic slot the last accessor step reads.
enum FactSelector {
    /// A static selector: the role text and the fact kind (the attribute
    /// name for a `.&name` write, empty for the node-fact roles).
    Static { role: String, kind: String },
    /// A dynamic selector (`PATH.@(expr)` / `PATH.&(expr)`): the operand is
    /// bound outside the write and resolved at run time; `attribute` marks
    /// the `.&` form.
    Dynamic { slot: VarSlot, attribute: bool },
}

impl FactSelector {
    fn role(&self) -> String {
        match self {
            Self::Static { role, .. } => role.clone(),
            Self::Dynamic { attribute, .. } => {
                if *attribute {
                    jqf_codec_core::markup::ATTRIBUTE_FACT.to_owned()
                } else {
                    String::new()
                }
            }
        }
    }

    fn kind(&self) -> String {
        match self {
            Self::Static { kind, .. } => kind.clone(),
            Self::Dynamic { .. } => String::new(),
        }
    }

    /// The node field's dynamic-selector slot: `None` for a static write.
    fn selector(&self) -> Option<(VarSlot, bool)> {
        match self {
            Self::Static { .. } => None,
            Self::Dynamic { slot, attribute } => Some((*slot, *attribute)),
        }
    }
}

/// The writable NODE-FACT vocabulary lives beside the executor's runtime
/// validator ([`crate::exec::WRITABLE_NODE_ROLES`]): the lowering validates
/// STATIC selectors against it, and the executor validates DYNAMIC write
/// selectors at run time against the same list.
use crate::exec::WRITABLE_NODE_ROLES;

/// One lowered fact-assignment target: the base expression, the prefix steps
/// before the accessor, and the accessor's selector.
struct FactTarget<'ast> {
    base: &'ast Expr,
    prefix: &'ast [PostfixStep],
    selector: FactSelector,
}

/// The FACT-ASSIGNMENT target shape: the target is a Postfix whose
/// LAST step is a node accessor (`.@role` / `.@["role"]` / `.@(expr)`) naming
/// a supported fact role, or an attribute accessor (`.&name` / `.&["name"]` /
/// `.&(expr)`). Returns the base expression, the prefix steps before the
/// accessor, and the selector. A dynamic selector's operand lowers through
/// [`lower_dynamic_accessor`]'s three arms — a hole-free string folds STATIC,
/// so `.@("comment") = v` compiles exactly as `.@comment = v` does; a real
/// expression is bound outside the write and validated at run time against
/// [`WRITABLE_NODE_ROLES`] — never a silent write.
///
/// Every other shape keeps the ordinary accessor-assignment rejection: a
/// non-admitted STATIC role name (`.@bogus`), and an accessor that is NOT the
/// last step (nested/dotted fact paths are ruled never — the role vocabulary
/// is flat by law). The prefix itself is lowered later by [`lower_fact_path`],
/// whose key/index-chain check rejects any accessor left inside it.
fn fact_assign_target<'ast>(
    target: &'ast Expr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
    binders: &mut Vec<OperandBinder>,
) -> Result<Option<FactTarget<'ast>>, EngineCompileError> {
    let ExprKind::Postfix(postfix) = target.kind() else {
        return Ok(None);
    };
    let Some((last, prefix)) = postfix.steps().split_last() else {
        return Ok(None);
    };
    let base = postfix.base();
    match &last.segment {
        PostfixSegment::NodeAccessor { selector } => {
            match lower_accessor_segment(selector, false, source, lowerer, binders)? {
                StepAccess::NodeAccessor(role) => {
                    if !WRITABLE_NODE_ROLES.contains(&role.as_str()) {
                        return Ok(None);
                    }
                    Ok(Some(FactTarget {
                        base,
                        prefix,
                        selector: FactSelector::Static {
                            role,
                            kind: String::new(),
                        },
                    }))
                }
                StepAccess::DynNodeAccessor(slot) => Ok(Some(FactTarget {
                    base,
                    prefix,
                    selector: FactSelector::Dynamic { slot, attribute: false },
                })),
                _ => Ok(None),
            }
        }
        PostfixSegment::Attribute { selector } => {
            match lower_accessor_segment(selector, true, source, lowerer, binders)? {
                StepAccess::Attribute(kind) => Ok(Some(FactTarget {
                    base,
                    prefix,
                    selector: FactSelector::Static {
                        role: jqf_codec_core::markup::ATTRIBUTE_FACT.to_owned(),
                        kind,
                    },
                })),
                StepAccess::DynAttribute(slot) => Ok(Some(FactTarget {
                    base,
                    prefix,
                    selector: FactSelector::Dynamic { slot, attribute: true },
                })),
                _ => Ok(None),
            }
        }
        PostfixSegment::Field { .. }
        | PostfixSegment::Index { .. }
        | PostfixSegment::Slice { .. }
        | PostfixSegment::ErrorSuppression => Ok(None),
    }
}

/// Lowers one edit-mode FACT assignment into a [`ProgramNode::FactAssign`],
/// mirroring [`lower_assignment`]'s operator law exactly: `=` binds its
/// right-hand side OUTSIDE the node, `|=` keeps the update as a filter, and
/// `op=`/`//=` compose the current payload with the bound value.
#[allow(
    clippy::too_many_lines,
    reason = "one lowering per operator family: the fact-assignment drive mirrors the \
              value-assignment drive's binding law arm for arm"
)]
fn lower_fact_assignment<'ast>(
    target: &FactTarget<'ast>,
    selector_binders: &[OperandBinder],
    assignment: &'ast AssignmentExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let (paths, mut binders) = lower_fact_path(target.base, target.prefix, source, lowerer)?;
    // The executor resolves the value path over the LOCATED input with a
    // key/index walk (static steps and runtime-resolved `DynVar` slots), so
    // only that shape is a legal fact target. An accessor
    // left in the prefix fails here too: an accessor step is never a path
    // component.
    if !is_key_index_chain(&lowerer.nodes, paths) {
        return Err(EngineCompileError::unsupported(
            assignment.op_span,
            UnsupportedConstruct::Expression(
                "a fact assignment over a non-static path (the fact target is a \
                 key/index chain ending in `.@comment`, `.&(name)`, or their dynamic forms)",
            ),
        ));
    }
    // The selector's own operand binders wrap INSIDE the path binders (the
    // accessor is the LAST step, so it varies fastest), and both wrap around
    // the FactAssign node itself: each operand output re-runs the whole write
    // against the ORIGINAL input, exactly the fan-out law a read's operands
    // follow.
    binders.extend(selector_binders.iter().copied());
    if assignment.op == AssignmentOp::Update {
        let update = lower_expr(&assignment.value, source, lowerer)?;
        let update = cap_at_first_output(update, lowerer)?;
        return push_fact_assign(lowerer, paths, &target.selector, update, ModifyMode::Update, &binders);
    }
    let bound_source = lower_expr(&assignment.value, source, lowerer)?;
    let slot = lowerer.scopes.allocate_anonymous()?;
    let bound = push_node(&mut lowerer.nodes, variable_stage(slot))?;
    let (update, mode) = match assignment.op {
        AssignmentOp::Assign => (bound, ModifyMode::Set),
        AssignmentOp::Alternative => {
            let current = push_node(&mut lowerer.nodes, current_stage())?;
            let update = push_node(
                &mut lowerer.nodes,
                ProgramNode::Alternative {
                    left: current,
                    right: bound,
                },
            )?;
            (update, ModifyMode::Update)
        }
        op => {
            let kind = arithmetic_update_kind(op).ok_or_else(|| {
                EngineCompileError::unsupported(
                    assignment.op_span,
                    UnsupportedConstruct::Expression("an unrecognized assignment operator"),
                )
            })?;
            let current = push_node(&mut lowerer.nodes, current_stage())?;
            let update = push_node(
                &mut lowerer.nodes,
                ProgramNode::Binary {
                    op: kind,
                    left: current,
                    right: bound,
                    shape: crate::program::BinaryShape::Framed,
                },
            )?;
            (update, ModifyMode::Update)
        }
    };
    let body = push_fact_assign(lowerer, paths, &target.selector, update, mode, &binders)?;
    push_node(
        &mut lowerer.nodes,
        ProgramNode::Bind {
            source: bound_source,
            slot,
            body,
            frame: false,
        },
    )
}

/// Pushes one lowered fact assignment's [`ProgramNode::FactAssign`] and wraps
/// the value-path and selector operand binders around it.
fn push_fact_assign(
    lowerer: &mut Lowerer<'_, '_>,
    paths: ProgramNodeId,
    selector: &FactSelector,
    update: ProgramNodeId,
    mode: ModifyMode,
    binders: &[OperandBinder],
) -> Result<ProgramNodeId, EngineCompileError> {
    let node = push_node(
        &mut lowerer.nodes,
        ProgramNode::FactAssign {
            paths,
            role: selector.role(),
            kind: selector.kind(),
            selector: selector.selector(),
            update,
            mode,
        },
    )?;
    wrap_operand_frames(node, binders.to_vec(), &mut lowerer.nodes)
}

/// Lowers one fact-assignment VALUE path: the base expression plus the postfix
/// steps BEFORE the accessor, through the ordinary postfix machinery (the
/// trimmed sibling of [`lower_postfix_expr`] — a fact target has no engine
/// term and no term-`?` split). Returns the bare chain AND its operand
/// binders UNWRAPPED: the caller wraps them around the
/// [`ProgramNode::FactAssign`] node itself, so each operand output re-runs
/// the whole write against the ORIGINAL input (multi-output paths fan out per
/// output, in order).
fn lower_fact_path<'ast>(
    base: &'ast Expr,
    steps: &'ast [PostfixStep],
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<(ProgramNodeId, Vec<OperandBinder>), EngineCompileError> {
    let current = lower_expr(base, source, lowerer)?;
    let mut pending: Vec<StageStep> = Vec::new();
    let mut binders: Vec<OperandBinder> = Vec::new();
    for step in steps {
        if matches!(step.segment, PostfixSegment::ErrorSuppression) {
            return Err(EngineCompileError::unsupported(
                step.span,
                UnsupportedConstruct::Expression("a term-`?` inside a fact assignment target"),
            ));
        }
        let optional = step.optional_suffix_span.is_some();
        let access = lower_segment(&step.segment, source, lowerer, &mut binders)?;
        pending
            .try_reserve(1)
            .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
        pending.push(StageStep::new(access, optional));
    }
    let chain = compose_postfix(current, pending, &mut lowerer.nodes)?;
    Ok((chain, binders))
}

/// Whether the graph at `id` is a bare input-start stage of only key/index
/// resolution steps — static [`StepAccess::Key`] / [`StepAccess::Index`] and
/// runtime-resolved [`StepAccess::DynVar`] slots (`.[$k]`).
/// This is the only fact-assignment value path the executor resolves (the `?`
/// flags are ignored, since a missing member is a missing path, not a
/// mismatch).
fn is_key_index_chain(nodes: &[ProgramNode], id: ProgramNodeId) -> bool {
    match nodes.get(id.index()) {
        Some(ProgramNode::Stage {
            start: StageStart::Current,
            steps,
        }) => steps.iter().all(|step| {
            matches!(
                step.access(),
                StepAccess::Key(_) | StepAccess::Index(_) | StepAccess::DynVar(_)
            )
        }),
        _ => false,
    }
}

/// The [`ProgramNode::Modify`] node, with its UPDATE capped at one output.
///
/// The cap is applied to [`ModifyMode::Update`] alone. A `Set` update is a bare
/// `$v` reference, which emits exactly one value by construction — wrapping it
/// would buy nothing and would cost one raised break marker per path.
fn modify_node(
    paths: ProgramNodeId,
    update: ProgramNodeId,
    mode: ModifyMode,
    lowerer: &mut Lowerer<'_, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let update = match mode {
        ModifyMode::Set => update,
        ModifyMode::Update => cap_at_first_output(update, lowerer)?,
    };
    push_node(&mut lowerer.nodes, ProgramNode::Modify { paths, update, mode })
}

/// The binary operator one arithmetic update assignment applies.
///
/// `AssignmentOp` is `#[non_exhaustive]`, so a form this table does not name is
/// rejected by span rather than silently mislowered.
const fn arithmetic_update_kind(op: AssignmentOp) -> Option<BinaryKind> {
    match op {
        AssignmentOp::Add => Some(BinaryKind::Add),
        AssignmentOp::Subtract => Some(BinaryKind::Subtract),
        AssignmentOp::Multiply => Some(BinaryKind::Multiply),
        AssignmentOp::Divide => Some(BinaryKind::Divide),
        AssignmentOp::Remainder => Some(BinaryKind::Remainder),
        _ => None,
    }
}

/// Lowers `limit($n; f)` into its definition, with the COUNTDOWN lowered to the
/// counted-stream node:
///
/// ```text
/// reference:   $n as the bound:
///         if $n > 0  then label $out
///                         | foreach f as $item ($n; . - 1;
///                             $item, if . <= 0 then break $out else empty end)
///         elif $n == 0 then empty
///         else error("limit doesn't support negative count") end
///
/// jqf:  the `label`/`foreach` pair  ->  Counted{Limit, $n, f}
/// ```
///
/// Every edge case this shape produces is INHERITED rather than reimplemented,
/// and the SUBSTITUTION is exactly the
/// countdown — the surrounding shape is untouched, which is what keeps the
/// inheritance intact: `$n` is still a value parameter bound OUTSIDE, so a
/// GENERATOR count runs the whole body once per count (`limit(1,2; 1,2,3)` →
/// `1,1,2`); the sign gates are still the `Binary` comparisons they were, so
/// `limit(0; f)` never evaluates `f` and a negative count still raises with
/// the wording, at the same position; and [`ProgramNode::Counted`] pays
/// the same `. - 1` per item, so a fractional count still yields `ceil(n)`
/// items and a non-numeric count still raises the ordinary subtraction type
/// error LAZILY (`limit("a"; empty)` is empty, not an error).
fn lower_limit<'ast>(
    call: &'ast CallExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let [count_arg, filter_arg] = call.args.as_slice() else {
        return Err(EngineCompileError::Parse(ParseRejection::internal(
            "limit/2 resolved without exactly two arguments",
        )));
    };
    let count = lower_expr(&count_arg.expression, source, lowerer)?;
    // `def limit($n; …)` is sugar for binding the count first, which is what
    // makes a multi-output count run the whole body once per value.
    let count_slot = lowerer.scopes.allocate_anonymous()?;
    let stop = lowerer.labels.allocate_anonymous()?;

    let filter = lower_expr(&filter_arg.expression, source, lowerer)?;

    let labelled = push_node(
        &mut lowerer.nodes,
        ProgramNode::Counted {
            source: filter,
            count: count_slot,
            kind: CountedKind::Limit,
            stop,
        },
    )?;

    // `elif $n == 0 then empty else error("…") end`
    let is_zero = var_compared_to_zero(BinaryKind::Equal, count_slot, lowerer)?;
    let zero_arm = push_node(&mut lowerer.nodes, ProgramNode::Empty)?;
    let negative_arm = negative_count_error(LIMIT_NEGATIVE, lowerer)?;
    let else_chain = push_node(
        &mut lowerer.nodes,
        ProgramNode::Conditional {
            condition: is_zero,
            consequent: zero_arm,
            alternative: negative_arm,
        },
    )?;

    // `if $n > 0 then … end`
    let is_positive = var_compared_to_zero(BinaryKind::Greater, count_slot, lowerer)?;
    let body = push_node(
        &mut lowerer.nodes,
        ProgramNode::Conditional {
            condition: is_positive,
            consequent: labelled,
            alternative: else_chain,
        },
    )?;
    push_node(
        &mut lowerer.nodes,
        ProgramNode::Bind {
            source: count,
            slot: count_slot,
            body,
            frame: false,
        },
    )
}

/// The refusal for a negative `limit` count.
const LIMIT_NEGATIVE: &str = "limit doesn't support negative count";
/// The refusal for a negative `skip` count.
const SKIP_NEGATIVE: &str = "skip doesn't support negative count";
/// The refusal for a negative `nth` index — a DIFFERENT noun from the other
/// two, and the reason the message is a parameter rather than a constant.
const NTH_NEGATIVE: &str = "nth doesn't support negative indices";

/// Lowers `nth($n)` into its definition: `.[$n]`.
///
/// The index is a BOUND value, so a generator index fans out (`nth(0,2)` reads
/// two positions) and a negative one wraps exactly as `.[-1]` does — which is
/// why this arity accepts what `nth/2` refuses.
fn lower_nth_index<'ast>(
    call: &'ast CallExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let [index_arg] = call.args.as_slice() else {
        return Err(EngineCompileError::Parse(ParseRejection::internal(
            "nth/1 resolved without exactly one argument",
        )));
    };
    let index = lower_expr(&index_arg.expression, source, lowerer)?;
    let slot = lowerer.scopes.allocate_anonymous()?;
    let mut steps = Vec::new();
    steps
        .try_reserve_exact(1)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    // Not optional: `[1,2] | nth("a")` raises the ordinary index mismatch.
    steps.push(StageStep::new(StepAccess::DynVar(slot), false));
    let body = push_node(
        &mut lowerer.nodes,
        ProgramNode::Stage {
            start: StageStart::Current,
            steps,
        },
    )?;
    push_node(
        &mut lowerer.nodes,
        ProgramNode::Bind {
            source: index,
            slot,
            body,
            frame: false,
        },
    )
}

/// Lowers `nth($n; g)` into its definition:
/// `if $n < 0 then error("nth doesn't support negative indices")
///  else first(skip($n; g)) end`.
///
/// Composing the two rather than hand-rolling a counter is what makes
/// `nth(5; 1,2,3)` EMPTY instead of `3` — the generator simply runs out before
/// the countdown does — and it is where the fractional index rounds the same
/// way `skip`'s does (`nth(1.5; 1,2,3)` is `2`).
fn lower_nth<'ast>(
    call: &'ast CallExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    lower_counted_stream(call, "nth/2", NTH_NEGATIVE, CountedKind::Nth, source, lowerer)
}

/// Lowers `skip($n; g)` into its definition:
/// `if $n < 0 then error("skip doesn't support negative count")
///  else foreach g as $item ($n; . - 1; if . < 0 then $item else empty end) end`.
///
/// The negative guard is EAGER (`skip(-1; empty)` raises, though the generator
/// never yields) while the type error is LAZY (`skip("a"; empty)` is empty,
/// because `"a" < 0` is false under the total order and the subtraction is
/// never reached). Both facts fall out of the shape and neither is coded here.
fn lower_skip<'ast>(
    call: &'ast CallExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    lower_counted_stream(call, "skip/2", SKIP_NEGATIVE, CountedKind::Skip, source, lowerer)
}

/// The `skip($n; g)` countdown both `skip/2` and `nth/2` are built from,
/// refusing a negative count with the CALLER'S message.
///
/// The countdown itself is [`ProgramNode::Counted`], and the two callers differ
/// only in its [`CountedKind`] — `nth/2`'s cap is the node's own cut rather than
/// a `label`/`break` pair around the loop, which keeps it INSIDE the count
/// binding where it belongs (a generator index runs the whole shape once per
/// index, so a cap outside the binding would let the first index's cut cancel
/// the rest: `[nth(1,0; 10,20,30)]` is `[20,10]`, not `[20]`).
fn lower_counted_stream<'ast>(
    call: &'ast CallExpr,
    who: &'static str,
    negative: &'static str,
    counted: CountedKind,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let [count_arg, filter_arg] = call.args.as_slice() else {
        return Err(EngineCompileError::Parse(ParseRejection::internal(who)));
    };
    let count = lower_expr(&count_arg.expression, source, lowerer)?;
    let count_slot = lowerer.scopes.allocate_anonymous()?;
    let stop = lowerer.labels.allocate_anonymous()?;

    let filter = lower_expr(&filter_arg.expression, source, lowerer)?;

    let stream = push_node(
        &mut lowerer.nodes,
        ProgramNode::Counted {
            source: filter,
            count: count_slot,
            kind: counted,
            stop,
        },
    )?;

    // `if $n < 0 then error("…") else … end`
    let is_negative = var_compared_to_zero(BinaryKind::Less, count_slot, lowerer)?;
    let refusal = negative_count_error(negative, lowerer)?;
    let body = push_node(
        &mut lowerer.nodes,
        ProgramNode::Conditional {
            condition: is_negative,
            consequent: refusal,
            alternative: stream,
        },
    )?;
    push_node(
        &mut lowerer.nodes,
        ProgramNode::Bind {
            source: count,
            slot: count_slot,
            body,
            frame: false,
        },
    )
}

/// An integer literal stage.
fn integer_stage(value: i64) -> ProgramNode {
    ProgramNode::Stage {
        start: StageStart::Literal(Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(
            value,
        )))),
        steps: Vec::new(),
    }
}

/// `$slot <op> 0`.
fn var_compared_to_zero(
    op: BinaryKind,
    slot: VarSlot,
    lowerer: &mut Lowerer<'_, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let left = push_node(&mut lowerer.nodes, variable_stage(slot))?;
    let right = push_node(&mut lowerer.nodes, integer_stage(0))?;
    push_node(&mut lowerer.nodes, ProgramNode::binary(op, left, right))
}

/// `error("<text>")` — the three counted builtins' refusal, each with its own
/// wording (`limit` and `skip` say "negative count", `nth` says "negative
/// indices").
fn negative_count_error(text: &str, lowerer: &mut Lowerer) -> Result<ProgramNodeId, EngineCompileError> {
    let message = push_node(
        &mut lowerer.nodes,
        ProgramNode::Stage {
            start: StageStart::Literal(literal_string(text, lowerer.resources)?),
            steps: Vec::new(),
        },
    )?;
    let mut args = Vec::new();
    args.try_reserve_exact(1)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    args.push(message);
    let record = resolve_builtin("error", 1)
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("error/1 is not registered")))?;
    push_node(
        &mut lowerer.nodes,
        ProgramNode::call(record.id, record.semantic_revision, args),
    )
}

/// Lowers `inside(xs)` into its definition, `. as $x | xs | contains($x)`.
///
/// The binding is what makes the SWAP work: `contains` reads the container as
/// its input and the inner value as its argument, so `inside` has to hold the
/// current input somewhere while it evaluates the container. The swap is
/// observable — `"a" | inside(1)` reports `number (1) and string ("a")`, naming
/// the argument first — and it follows from the definition rather than from a
/// second refusal written here.
fn lower_inside<'ast>(
    call: &'ast CallExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let argument = call
        .args
        .first()
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("inside/1 resolved with no argument")))?;
    let inner = lowerer.scopes.allocate_anonymous()?;
    let containers = lower_expr(&argument.expression, source, lowerer)?;
    let bound = push_node(&mut lowerer.nodes, variable_stage(inner))?;
    let question = builtin_call("contains", &[bound], lowerer)?;
    let body = push_node(
        &mut lowerer.nodes,
        ProgramNode::FlatMap {
            upstream: containers,
            body: question,
        },
    )?;
    let current = push_node(&mut lowerer.nodes, current_stage())?;
    push_node(
        &mut lowerer.nodes,
        ProgramNode::Bind {
            source: current,
            slot: inner,
            body,
            frame: false,
        },
    )
}

/// A one-step `.[]` iteration step list for the `map` expansion.
fn each_steps() -> Result<Vec<StageStep>, EngineCompileError> {
    let mut steps = Vec::new();
    steps
        .try_reserve_exact(1)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    steps.push(StageStep::new(StepAccess::Each, false));
    Ok(steps)
}

/// Maps a parser binary operator to this vertical's [`BinaryKind`], or `None` for
/// an operator another vertical owns (pipe, comma, `and`/`or`/`//`).
fn arithmetic_binary_kind(op: BinaryOp) -> Option<BinaryKind> {
    Some(match op {
        BinaryOp::Add => BinaryKind::Add,
        BinaryOp::Subtract => BinaryKind::Subtract,
        BinaryOp::Multiply => BinaryKind::Multiply,
        BinaryOp::Divide => BinaryKind::Divide,
        BinaryOp::Remainder => BinaryKind::Remainder,
        BinaryOp::Equal => BinaryKind::Equal,
        BinaryOp::NotEqual => BinaryKind::NotEqual,
        BinaryOp::Less => BinaryKind::Less,
        BinaryOp::LessEqual => BinaryKind::LessEqual,
        BinaryOp::Greater => BinaryKind::Greater,
        BinaryOp::GreaterEqual => BinaryKind::GreaterEqual,
        // Pipe/comma, `and`/`or`/`//`, and any future operator are owned
        // elsewhere (or rejected by name).
        _ => return None,
    })
}

/// Maps a parser binary operator to this vertical's [`LogicalOp`], or `None` for
/// an operator that is not `and`/`or`.
fn logical_operator(op: BinaryOp) -> Option<LogicalOp> {
    match op {
        BinaryOp::And => Some(LogicalOp::And),
        BinaryOp::Or => Some(LogicalOp::Or),
        _ => None,
    }
}

/// Lowers `if C then A elif C2 then B … else D end` into nested
/// [`ProgramNode::Conditional`] nodes.
///
/// The authored branch vector desugars right-to-left: the innermost `alternative`
/// is the authored `else` branch, or a synthesized identity stage when no `else`
/// was written (`if .a then 1 end` on `{"a":false}` → the input). Each
/// earlier branch wraps the accumulated alternative, so `if c1 then t1 elif c2
/// then t2 else e end` becomes `Conditional(c1, t1, Conditional(c2, t2, e))`.
fn lower_conditional<'ast>(
    conditional: &'ast ConditionalExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let mut alternative = match &conditional.else_branch {
        Some(else_branch) => lower_expr(else_branch, source, lowerer)?,
        None => push_node(&mut lowerer.nodes, current_stage())?,
    };
    for branch in conditional.branches.iter().rev() {
        // The constant-result fold for `if`: a CONSTANT condition makes the
        // branch choice compile-time, so no
        // `Conditional` node is built. Truthiness is the one jqf law: only
        // `false` and `null` are falsy (semantics/truth.rs).
        if let Ok(value) = evaluate_constant(&branch.condition, source) {
            let falsy = matches!(value.untagged(), Value::Null | Value::Bool(false));
            if falsy {
                // BOTH arms compile, so an undefined variable in the DISCARDED
                // arm is still the compile-time `$x is not defined`
                // (`if null then $x else 1 end`). The constant fold must not
                // skip it: lower the dead arm for validation and drop the node.
                let _ = lower_expr(&branch.then_branch, source, lowerer)?;
                continue;
            }
            alternative = lower_expr(&branch.then_branch, source, lowerer)?;
            continue;
        }
        let condition = lower_expr(&branch.condition, source, lowerer)?;
        let consequent = lower_expr(&branch.then_branch, source, lowerer)?;
        alternative = push_node(
            &mut lowerer.nodes,
            ProgramNode::Conditional {
                condition,
                consequent,
                alternative,
            },
        )?;
    }
    Ok(alternative)
}

/// A scalar-literal producer: a [`StageStart::Literal`] stage with no steps.
fn literal_stage(value: Value) -> ProgramNode {
    ProgramNode::Stage {
        start: StageStart::Literal(value),
        steps: Vec::new(),
    }
}

/// Whether an expression is `.` however it was spelled — bare, parenthesised,
/// or piped into itself. Only these three: anything that could publish a
/// different value, or publish it a different number of times, is not the unit.
fn is_identity(expr: &Expr) -> bool {
    match expr.kind() {
        ExprKind::Identity => true,
        ExprKind::Group { expression, .. } => is_identity(expression),
        ExprKind::Binary(binary) if binary.op == BinaryOp::Pipe => {
            is_identity(&binary.left) && is_identity(&binary.right)
        }
        _ => false,
    }
}

/// An identity producer: a [`StageStart::Current`] stage with no steps.
const fn current_stage() -> ProgramNode {
    ProgramNode::Stage {
        start: StageStart::Current,
        steps: Vec::new(),
    }
}

/// A bound-variable producer: a [`StageStart::Variable`] stage with no steps yet
/// (a postfix chain on `$x` fuses its steps onto this same stage).
const fn variable_stage(slot: VarSlot) -> ProgramNode {
    ProgramNode::Stage {
        start: StageStart::Variable(slot),
        steps: Vec::new(),
    }
}

/// Lowers a string literal, WITH its interpolation, into a producer graph.
///
/// `"a\(x)b\(y)"` is one [`ProgramNode::Concat`] over the parts
/// `"a"`, `x|tostring`, `"b"`, `y|tostring`. The node's Cartesian product
/// is RIGHT-outer (the LEFTMOST part is the inner, fastest-varying loop),
/// so `"\(1,2)\(3,4)\(5,6)"` emits `"135" "235" "145" "245" "136" …` — the
/// first hole varying fastest, which is the reference order byte for byte
/// and the same order a left-associative `+` chain produced.
///
/// No `""` seed is prepended: every hole is `tostring`ed, so every part is
/// already a string and a leading hole needs no coercion partner.
/// A template with no holes never builds a concat at all — it is one literal.
/// A single remaining part (one hole, no literal text) is that part itself.
fn lower_string_template<'ast>(
    template: &'ast StringTemplate,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    if let Some(text) = static_template_text(template, source)? {
        let literal = literal_string(&text, lowerer.resources)?;
        return push_node(&mut lowerer.nodes, literal_stage(literal));
    }
    let mut parts = Vec::new();
    for segment in template.segments() {
        parts.push(lower_template_segment(segment, source, lowerer)?);
    }
    push_concat(parts, lowerer)
}

/// One Concat node over `parts`, or the single part when there is only one.
fn push_concat(parts: Vec<ProgramNodeId>, lowerer: &mut Lowerer<'_, '_>) -> Result<ProgramNodeId, EngineCompileError> {
    match parts.len() {
        0 => Err(EngineCompileError::Parse(ParseRejection::internal(
            "an interpolated template lowered to no parts",
        ))),
        1 => Ok(parts[0]),
        _ => push_node(&mut lowerer.nodes, ProgramNode::Concat { parts }),
    }
}

/// One link of the concatenation chain: a literal segment as itself, an
/// interpolation hole as `hole | tostring`.
fn lower_template_segment<'ast>(
    segment: &'ast TemplateSegment,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    match segment {
        TemplateSegment::Literal { span } => {
            let mut text = String::new();
            decode_literal_segment(*span, source, &mut text)?;
            let literal = literal_string(&text, lowerer.resources)?;
            push_node(&mut lowerer.nodes, literal_stage(literal))
        }
        TemplateSegment::Expression { expression, .. } => {
            let hole = lower_expr(expression, source, lowerer)?;
            let stringify = builtin_call("tostring", &[], lowerer)?;
            push_node(
                &mut lowerer.nodes,
                ProgramNode::FlatMap {
                    upstream: hole,
                    body: stringify,
                },
            )
        }
        _ => Err(EngineCompileError::unsupported(
            segment.span(),
            UnsupportedConstruct::Expression("an unsupported string segment"),
        )),
    }
}

/// Lowers `@name` into `format("name")`.
///
/// The name is carried as a LITERAL argument rather than resolved here, because
/// the name resolves at RUN time: `@frobnicate` compiles fine and raises
/// `frobnicate is not a valid format` only when a value reaches it. Keeping the
/// name in the argument keeps that timing, and keeps one implementation for both
/// spellings — `@json` and `format("json")` are the same call node.
fn lower_format(
    span: Span,
    source: &SyntaxSource<'_>,
    lowerer: &mut Lowerer<'_, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let name = format_name(span, source, lowerer.resources)?;
    let literal = push_node(&mut lowerer.nodes, literal_stage(name))?;
    builtin_call("format", &[literal], lowerer)
}

/// Lowers `@name "text\(hole)"` into the concatenation chain whose HOLES are
/// formatted and whose literal text is not.
///
/// That asymmetry is the whole point of the form: `@uri "http://x/?q=\("a b")"`
/// keeps its `://` and `?` intact and encodes only the interpolated part. It also
/// explains why an invalid name is harmless without holes — `@frobnicate "x"` is
/// exit 0 because the chain below never builds a `format` call for a template
/// that has nothing to format.
fn lower_format_template<'ast>(
    name_span: Span,
    template: &'ast StringTemplate,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    if let Some(text) = static_template_text(template, source)? {
        let literal = literal_string(&text, lowerer.resources)?;
        return push_node(&mut lowerer.nodes, literal_stage(literal));
    }
    let name = format_name(name_span, source, lowerer.resources)?;
    let mut parts = Vec::new();
    for segment in template.segments() {
        let part = match segment {
            TemplateSegment::Expression { expression, .. } => {
                let hole = lower_expr(expression, source, lowerer)?;
                let literal = push_node(&mut lowerer.nodes, literal_stage(name.clone()))?;
                let apply = builtin_call("format", &[literal], lowerer)?;
                push_node(
                    &mut lowerer.nodes,
                    ProgramNode::FlatMap {
                        upstream: hole,
                        body: apply,
                    },
                )?
            }
            other => lower_template_segment(other, source, lowerer)?,
        };
        parts.push(part);
    }
    push_concat(parts, lowerer)
}

/// The format name a `@name` token spells, as an owned string literal: the token
/// text without its `@`.
fn format_name(
    span: Span,
    source: &SyntaxSource<'_>,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineCompileError> {
    let token = source
        .text()
        .get(span.range())
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("format token span out of range")))?;
    let name = token
        .strip_prefix('@')
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("a format token without its `@`")))?;
    literal_string(name, resources)
}

/// One owned string literal for the arena. No ledger charge is taken here —
/// lowering never charges; the context only classifies an allocation refusal.
fn literal_string(text: &str, _resources: &ResourceContext<'_>) -> Result<Value, EngineCompileError> {
    Value::try_string(text).map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))
}

/// Applies the `_negate` value law to an already-parsed numeric literal, so a
/// folded `-<number>` term and the runtime operator cannot disagree.
fn negated_literal(magnitude: Value) -> Result<Value, EngineCompileError> {
    let Value::Number(number) = magnitude else {
        return Err(EngineCompileError::Parse(ParseRejection::internal(
            "a numeric literal did not lower to a number",
        )));
    };
    let negated = number
        .try_negated()
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    Ok(Value::Number(negated))
}

/// Parses a numeric literal spelling at `span` into an owned retained-spelling
/// [`Value::Number`] (the retained-spelling normalization), optionally
/// negated.
/// Lowers one object member into a key producer paired with a value producer.
///
/// A static-identifier or string key becomes a singleton [`StageStart::Literal`]
/// string producer; a `(expr)` key becomes `expr`'s graph. Shorthand (`{a}`,
/// `{"a b"}`) fills the value with `.<key>`.
///
/// A `$var` key is TWO different members depending on whether a value follows,
/// and the difference is which side the variable lands on:
///
/// * `{$x}` is `{"x": $x}` — the key is the identifier with the `$` STRIPPED, as
///   a literal, and the value is the bound slot. It does not read the input at
///   all, which is why `1 as $x | {$x}` is `{"x":1}` rather than `{"x":null}`.
/// * `{$x: e}` is `{($x): e}` — the variable is the KEY producer, so a non-string
///   binding is the constructor's own `Cannot use … as object key` refusal at run
///   time (`1 as $x | {$x: 9}`).
fn lower_object_member<'ast>(
    member: &'ast ObjectMember,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ObjectMemberNode, EngineCompileError> {
    match &member.key {
        ObjectKey::Name(span) => {
            let name = identifier_text(*span, source)?;
            lower_static_key_member(name, member.value.as_ref(), source, lowerer)
        }
        ObjectKey::String(template) => match static_template_text(template, source)? {
            Some(name) => lower_static_key_member(name, member.value.as_ref(), source, lowerer),
            None => lower_interpolated_key_member(template, member.value.as_ref(), source, lowerer),
        },
        ObjectKey::Variable(span) => lower_variable_key_member(*span, member.value.as_ref(), source, lowerer),
        ObjectKey::Expr(expr) => {
            let key = lower_expr(expr, source, lowerer)?;
            let Some(value) = member.value.as_ref() else {
                // Format-template shorthand `{@text "k"}`: the format is the
                // key producer; the value looks that key up on the constructor
                // input. Same once-bound lookup as interpolated-string
                // shorthand — see [`lower_interpolated_key_member`].
                return lower_bound_key_lookup(key, lowerer);
            };
            let value = lower_expr(value, source, lowerer)?;
            Ok(ObjectMemberNode {
                key,
                value,
                static_key: None,
            })
        }
        _ => Err(EngineCompileError::unsupported(
            member.span,
            UnsupportedConstruct::Expression("an unsupported object member key"),
        )),
    }
}

/// Lowers `{$x}` and `{$x: v}`.
///
/// Both resolve the variable first, so an undefined one is named at its own span
/// (and `{$__loc__}` keeps reporting the `$__loc__` refusal rather than a generic
/// undefined-variable error). Then the two forms diverge: with a value the
/// binding is the KEY producer, without one it is the VALUE and the key is the
/// `$`-stripped identifier as a literal.
fn lower_variable_key_member<'ast>(
    span: Span,
    value: Option<&'ast Expr>,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ObjectMemberNode, EngineCompileError> {
    // A data-import variable (`$d`) lowers to a literal producer here too:
    // `{$d}` must see the module's data, not the ordinary scope.
    let sigiled = source
        .text()
        .get(span.range())
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("object `$var` key span out of range")))?;
    if let Some(position) = lowerer.module_vars.iter().rposition(|(bound, _)| bound == sigiled) {
        let name = variable_key_text(span, source)?;
        let key_literal = literal_string(name, lowerer.resources)?;
        let key = push_node(&mut lowerer.nodes, literal_stage(key_literal))?;
        let data = lowerer.module_vars[position].1.clone();
        let value = match value {
            Some(expr) => lower_expr(expr, source, lowerer)?,
            None => push_node(&mut lowerer.nodes, literal_stage(data))?,
        };
        return Ok(ObjectMemberNode {
            key,
            value,
            static_key: None,
        });
    }
    if is_loc_binding(span, source) {
        let name = variable_key_text(span, source)?;
        let key_literal = literal_string(name, lowerer.resources)?;
        let key = push_node(&mut lowerer.nodes, literal_stage(key_literal))?;
        #[expect(
            clippy::single_match_else,
            reason = "kept in the same `explicit value, else the binding's implied default` shape as the sibling $__loc__ and variable-shorthand members above"
        )]
        let value = match value {
            Some(expr) => lower_expr(expr, source, lowerer)?,
            None => {
                let location = location_literal(span, source, lowerer.resources)?;
                push_node(&mut lowerer.nodes, literal_stage(location))?
            }
        };
        return Ok(ObjectMemberNode {
            key,
            value,
            static_key: None,
        });
    }
    if is_env_variable(span, source) {
        // `$ENV` is not a binder slot — it lowers to the `env/0` call, the
        // same law a bare `$ENV` uses. `{$ENV}` is the shorthand
        // `{"ENV": env}`; `{$ENV: 1}` is a COMPUTED-key member whose key
        // producer is the env object — the runtime `Cannot use object … as
        // object key`, exactly as for any non-string computed key.
        let env = lower_env_call(lowerer)?;
        return if let Some(expr) = value {
            let value = lower_expr(expr, source, lowerer)?;
            Ok(ObjectMemberNode {
                key: env,
                value,
                static_key: None,
            })
        } else {
            let name = variable_key_text(span, source)?;
            let key_literal = literal_string(name, lowerer.resources)?;
            let key = push_node(&mut lowerer.nodes, literal_stage(key_literal))?;
            Ok(ObjectMemberNode {
                key,
                value: env,
                static_key: None,
            })
        };
    }
    let slot = resolve_variable(span, source, &lowerer.scopes)?;
    if let Some(expr) = value {
        let key = push_node(&mut lowerer.nodes, variable_stage(slot))?;
        let value = lower_expr(expr, source, lowerer)?;
        return Ok(ObjectMemberNode {
            key,
            value,
            static_key: None,
        });
    }
    let name = variable_key_text(span, source)?;
    let key_literal = literal_string(name, lowerer.resources)?;
    let key = push_node(&mut lowerer.nodes, literal_stage(key_literal))?;
    let value = push_node(&mut lowerer.nodes, variable_stage(slot))?;
    Ok(ObjectMemberNode {
        key,
        value,
        static_key: None,
    })
}

/// The member NAME a `{$var}` shorthand spells: the reference's text without its
/// leading `$`. `{$foreach}` is the key `"foreach"`, keywords included.
fn variable_key_text<'text>(span: Span, source: &'text SyntaxSource<'_>) -> Result<&'text str, EngineCompileError> {
    source
        .text()
        .get(span.range())
        .and_then(|text| text.strip_prefix('$'))
        .ok_or_else(|| {
            EngineCompileError::Parse(ParseRejection::internal(
                "object `$var` key span is not a variable reference",
            ))
        })
}

/// Lowers `{"a\(e)": v}` and its shorthand `{"a\(e)"}`.
///
/// With an explicit value the key is just a producer, exactly like `{(expr): v}`.
///
/// The SHORTHAND is the interesting one: `{"a\(e)"}` reads `.["a\(e)"]`, and
/// the key is evaluated ONCE — `{"a\(1,2)"}` emits two objects, not four — so the
/// value cannot simply re-lower the template. The key is therefore bound: the key
/// producer is `Bind { source: template, slot, body: $slot }`, which emits the key
/// AND leaves it in `slot`, and the value producer reads `.[$slot]`. That the slot
/// is still readable when the value producer runs is the binder's documented
/// invariant, not luck — [`ProgramNode::Bind`] gives each binder OCCURRENCE its
/// own slot with no save/restore precisely so that downstream code may run while
/// the binder frame is live.
///
/// Binding inside the key producer, rather than wrapping the whole constructor,
/// is what keeps the fan-out order: `{"a\(1,2)":9,"b\(3,4)"}` varies the FIRST
/// member's key slowest, which is `ConstructObject`'s own member order, and a
/// constructor-level wrapper would invert it.
fn lower_interpolated_key_member<'ast>(
    template: &'ast StringTemplate,
    value: Option<&'ast Expr>,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ObjectMemberNode, EngineCompileError> {
    let text = lower_string_template(template, source, lowerer)?;
    let Some(value) = value else {
        return lower_bound_key_lookup(text, lowerer);
    };
    let value = lower_expr(value, source, lowerer)?;
    Ok(ObjectMemberNode {
        key: text,
        value,
        static_key: None,
    })
}

/// Binds a computed key once and looks it up on the constructor input.
///
/// Shared by interpolated-string shorthand `{"a\(e)"}` and format-template
/// shorthand `{@text "k"}`. The once-evaluation and fan-out laws live on
/// [`lower_interpolated_key_member`].
fn lower_bound_key_lookup(
    key: ProgramNodeId,
    lowerer: &mut Lowerer<'_, '_>,
) -> Result<ObjectMemberNode, EngineCompileError> {
    let slot = lowerer.scopes.allocate_anonymous()?;
    let emit = push_node(&mut lowerer.nodes, variable_stage(slot))?;
    let key = push_node(
        &mut lowerer.nodes,
        ProgramNode::Bind {
            source: key,
            slot,
            body: emit,
            frame: false,
        },
    )?;
    let value = push_node(
        &mut lowerer.nodes,
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: single_dyn_step(slot)?,
        },
    )?;
    Ok(ObjectMemberNode {
        key,
        value,
        static_key: None,
    })
}

/// A one-step `.[$slot]` step list.
fn single_dyn_step(slot: VarSlot) -> Result<Vec<StageStep>, EngineCompileError> {
    let mut steps = Vec::new();
    steps
        .try_reserve_exact(1)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    steps.push(StageStep::new(StepAccess::DynVar(slot), false));
    Ok(steps)
}

/// Lowers a static-keyed member: a singleton literal-string key producer, plus
/// either the explicit value graph or the shorthand `.<key>` path.
fn lower_static_key_member<'ast>(
    name: String,
    value: Option<&'ast Expr>,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ObjectMemberNode, EngineCompileError> {
    let key_literal = literal_string(&name, lowerer.resources)?;
    let key = push_node(&mut lowerer.nodes, literal_stage(key_literal))?;
    let value = match value {
        Some(expr) => lower_expr(expr, source, lowerer)?,
        // Shorthand `{a}` ≡ `{a: .a}`: the value is the `.name` path.
        None => push_node(
            &mut lowerer.nodes,
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: single_key_step(name)?,
            },
        )?,
    };
    Ok(ObjectMemberNode {
        key,
        value,
        static_key: None,
    })
}

/// A one-step `.name` path list for object shorthand.
fn single_key_step(name: String) -> Result<Vec<StageStep>, EngineCompileError> {
    let mut steps = Vec::new();
    steps
        .try_reserve_exact(1)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    steps.push(StageStep::new(StepAccess::Key(name), false));
    Ok(steps)
}

/// The identifier text at `span` copied into an owned [`String`].
fn identifier_text(span: Span, source: &SyntaxSource<'_>) -> Result<String, EngineCompileError> {
    let text = source
        .text()
        .get(span.range())
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("object key name span out of range")))?;
    copy_string(text)
}

/// Copies `text` into an owned [`String`] with a fallible reservation.
fn copy_string(text: &str) -> Result<String, EngineCompileError> {
    try_copy_str(text).ok_or(EngineCompileError::Resource(ResourceError::AllocationFailed))
}

/// Lowers a postfix chain, composing its steps onto its base and splitting each
/// term-level error-suppression `?` into a catchless `try` barrier.
///
/// The base is ANY expression — identity (`.a`), a group (`(.a, .b).c`), a scalar
/// literal (`3[]?`), a constructor (`{a: .b}.a`), a call, a conditional
/// (`if true then [.] else . end []`), a fold (`(reduce .[] as $x (0; .))[0]`) —
/// because a base carries no law of its own; see [`lower_postfix_base`]. A
/// [`PostfixSegment::ErrorSuppression`]
/// segment (`(.a)?`, the second `?` of `.a??`, `.?`) is the whole-term `try`
/// SUGAR: its PREFIX (base + steps so far) lowers INTO the `try` body and
/// its SUFFIX composes on the `try`'s outputs OUTSIDE the barrier (`(.a)?.b` and
/// `.a??.b` both error at `.b`).
fn lower_postfix_expr<'ast>(
    postfix: &'ast PostfixExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let base = postfix.base();
    // The engine-binding projection: `~x.next` / `~x.rest` are the ONLY
    // expressions whose base is an engine binding — the base itself never
    // lowers as a value (the value-return guard).
    if let ExprKind::EngineTerm { .. } = base.kind() {
        return lower_engine_pull(postfix, source, lowerer);
    }
    let mut current = lower_expr(base, source, lowerer)?;
    let mut pending: Vec<StageStep> = Vec::new();
    let mut binders: Vec<OperandBinder> = Vec::new();
    for step in postfix.steps() {
        if matches!(step.segment, PostfixSegment::ErrorSuppression) {
            // Term-try split: flush the accumulated prefix into the body, wrap it in
            // a catchless `try`, and continue composing the suffix OUTSIDE it. The
            // prefix's OPERAND frames are flushed with it, because a term `?` covers
            // the operands its prefix authored (see [`bind_operand`]).
            current = compose_postfix(current, core::mem::take(&mut pending), &mut lowerer.nodes)?;
            current = wrap_operand_frames(current, core::mem::take(&mut binders), &mut lowerer.nodes)?;
            current = push_node(
                &mut lowerer.nodes,
                ProgramNode::Try {
                    body: current,
                    handler: None,
                },
            )?;
            continue;
        }
        let optional = step.optional_suffix_span.is_some();
        let access = lower_segment(&step.segment, source, lowerer, &mut binders)?;
        pending
            .try_reserve(1)
            .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
        pending.push(StageStep::new(access, optional));
    }
    let chain = compose_postfix(current, pending, &mut lowerer.nodes)?;
    wrap_operand_frames(chain, binders, &mut lowerer.nodes)
}

/// The accessor WRITE half, walked on assignment/update targets: any postfix
/// node/attribute accessor step (`.@` / `.&`) in the path. [`lower_assignment`]
/// routes what it finds to [`lower_fact_assignment`] (the accessor as the
/// target's LAST step) or the compile-time rejection (an accessor anywhere
/// else).
fn find_accessor_step(expr: &Expr) -> Option<Span> {
    match &expr.kind() {
        ExprKind::Postfix(postfix) => {
            for step in postfix.steps() {
                if matches!(
                    step.segment,
                    PostfixSegment::NodeAccessor { .. } | PostfixSegment::Attribute { .. }
                ) {
                    return Some(step.span);
                }
            }
            find_accessor_step(postfix.base())
        }
        ExprKind::Group { expression, .. } => find_accessor_step(expression.as_ref()),
        _ => None,
    }
}

/// Lowers an engine-binding PULL — the only value-producing use of a `~x`
/// binding, and one of exactly two projections (`~x.next` and `~x.rest`).
///
/// The chain must be EXACTLY one projection step (the protocol is closed), the
/// name must resolve against the engine scope, and the pull must not sit inside
/// a recursive callable body (the carve-out) or a `~generator` argument
/// (cross-machine capture) — both are lower-time typed rejections, never a
/// runtime surprise.
fn lower_engine_pull<'ast>(
    postfix: &'ast PostfixExpr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let ExprKind::EngineTerm { tilde_span, name } = postfix.base().kind() else {
        return Err(EngineCompileError::Parse(ParseRejection::internal(
            "engine pull lowered from a non-engine base",
        )));
    };
    let name_text = source
        .text()
        .get(name.range())
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("engine binding name span out of range")))?;
    let full = alloc::format!("~{name_text}");
    let span = tilde_span.merge(*name);
    let kind = match postfix.steps() {
        [step] => match &step.segment {
            PostfixSegment::Field {
                selector: FieldSelector::Name(selector),
            } => match source.text().get(selector.range()) {
                Some("next") => EnginePullKind::Next,
                Some("rest") => EnginePullKind::Rest,
                _ => {
                    return Err(EngineCompileError::engine_binding_projection(
                        span.merge(step.span),
                        &full,
                    ));
                }
            },
            _ => {
                return Err(EngineCompileError::engine_binding_projection(
                    span.merge(step.span),
                    &full,
                ));
            }
        },
        _ => {
            return Err(EngineCompileError::engine_binding_projection(
                span.merge(postfix.steps().last().map_or(*name, |step| step.span)),
                &full,
            ));
        }
    };
    // A pull inside a `~generator` argument would be evaluated by the cursor's
    // OWN machine, whose cursor store cannot hold the enclosing machine's
    // cursor — reject the cross-machine capture at lower time.
    if lowerer.in_engine_constructor > 0 {
        return Err(EngineCompileError::engine_binding_in_constructor(span, &full));
    }
    // The carve-out: a recursive `def` body runs on a nested evaluator with no
    // cursor store. The rejection is a compile error naming the restriction,
    // never a hang and never a wrong answer.
    if lowerer.callable_depth > 0 {
        return Err(EngineCompileError::engine_pull_in_recursive_def(span, &full));
    }
    let Some(slot) = lowerer.engine_scopes.resolve(name_text) else {
        return Err(EngineCompileError::undefined_engine_binding(span, &full));
    };
    push_node(&mut lowerer.nodes, ProgramNode::EnginePull { slot, kind })
}

/// One general-expression step operand hoisted out of its path step: the
/// anonymous slot the step reads, and the graph that fills it.
#[derive(Clone, Copy)]
struct OperandBinder {
    slot: VarSlot,
    source: ProgramNodeId,
}

/// Hoists a general-expression step operand into a fresh anonymous binder,
/// recording the frame for [`wrap_operand_frames`] and answering the slot the
/// step will read.
///
/// The operand's graph is lowered HERE, at the postfix chain's own scope and with
/// dot meaning the chain's input, because [`ProgramNode::Bind`] runs its source
/// with dot unchanged and the frame wraps the whole chain: `.a[.b]` resolves `.b`
/// against the input, not against `.a`. The slot is anonymous
/// because no user source can name it — and it takes no scope extent, so it is
/// never paired with a `pop` (see [`Scopes::allocate_anonymous`]).
///
/// The frame's placement carries one law: a TERM-level `?` covers the
/// operand's own error (`[(.[error("x")])?]` is `[]`, `[.[error("x")]??]` is
/// `[]`) while a per-component `?` does NOT (`[.[error("x")]?]` raises). Frames
/// therefore sit INSIDE the term-try barrier and OUTSIDE the `Stage` that carries
/// the per-component flags.
fn bind_operand<'ast>(
    operand: &'ast Expr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
    binders: &mut Vec<OperandBinder>,
) -> Result<VarSlot, EngineCompileError> {
    let graph = lower_expr(operand, source, lowerer)?;
    bind_operand_graph(graph, lowerer, binders)
}

/// [`bind_operand`] for an operand whose graph is already lowered — an
/// interpolated field key, which is a template rather than an [`Expr`].
fn bind_operand_graph(
    graph: ProgramNodeId,
    lowerer: &mut Lowerer<'_, '_>,
    binders: &mut Vec<OperandBinder>,
) -> Result<VarSlot, EngineCompileError> {
    let slot = lowerer.scopes.allocate_anonymous()?;
    binders
        .try_reserve(1)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    binders.push(OperandBinder { slot, source: graph });
    Ok(slot)
}

/// Nests the recorded operand frames around an already-composed chain, innermost
/// first.
///
/// The nesting order IS the fan-out order, and both directions come from one
/// rule: the LATER a generator's forks are created, the FASTER it varies. The
/// chain's operands are generated last-bracket-first, and `start` before
/// `end` within one slice, so outermost→innermost is (last step … first step)
/// with `start` outside `end`. `[.[0,1][0,1]]` on `[[10,11],[20,21]]` is
/// `[10,20,11,21]` (last bracket slowest) and `[.[(0,1):(3,4)]]` on `[0,1,2,3,4]`
/// is `[[0,1,2],[0,1,2,3],[1,2],[1,2,3]]` (start slowest).
///
/// [`lower_segment`] records frames in the order that makes this loop a forward
/// walk: one per index step, and `end` BEFORE `start` for a slice.
fn wrap_operand_frames(
    body: ProgramNodeId,
    binders: Vec<OperandBinder>,
    nodes: &mut Vec<ProgramNode>,
) -> Result<ProgramNodeId, EngineCompileError> {
    let mut current = body;
    for binder in binders {
        current = push_node(
            nodes,
            ProgramNode::Bind {
                source: binder.source,
                slot: binder.slot,
                body: current,
                frame: false,
            },
        )?;
    }
    Ok(current)
}

/// Lowers a postfix chain's base to an arena node — through [`lower_expr`], for
/// EVERY base form.
///
/// There is no base-position law of its own: `BASE STEPS` is `BASE | .STEPS`, and
/// [`compose_postfix`] already carries the only distinction that matters (a
/// `Stage` base fuses the steps onto itself; anything else becomes a `FlatMap`).
/// So `if true then [.] else . end []` iterates the branch's value,
/// `(reduce .[] as $x (0; .))[0]` indexes the fold's result, and `(-[1])[0]`
/// reports the negation's refusal — each one composition, not a special case.
///
/// The function stays as its own named step because the base is the one position
/// Composes a postfix `steps` list onto an already-lowered base graph.
///
/// An empty step list leaves the base unchanged (a term-try wrap with no trailing
/// suffix). A `Stage` base fuses the steps directly onto its step list (the path
/// composition law); any other base becomes `FlatMap(base, Stage[steps])`, run
/// once per value the base produces (`(.a, .b).c` ≡ `(.a, .b) | .c`).
fn compose_postfix(
    base_root: ProgramNodeId,
    steps: Vec<StageStep>,
    nodes: &mut Vec<ProgramNode>,
) -> Result<ProgramNodeId, EngineCompileError> {
    if steps.is_empty() {
        return Ok(base_root);
    }
    if let ProgramNode::Stage { steps: base_steps, .. } = &mut nodes[base_root.index()] {
        base_steps
            .try_reserve(steps.len())
            .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
        base_steps.extend(steps);
        return Ok(base_root);
    }
    let body = push_node(
        nodes,
        ProgramNode::Stage {
            start: StageStart::Current,
            steps,
        },
    )?;
    push_node(
        nodes,
        ProgramNode::FlatMap {
            upstream: base_root,
            body,
        },
    )
}

/// Appends `node` to the arena and returns its dense id.
///
/// The single door into the arena, and therefore where [`MAX_LOWERED_NODES`] holds
/// for every construct at once. Before this, only definition inlining asked, so a
/// lowering that multiplied elsewhere grew until the ALLOCATOR refused: a `?//`
/// chain nested `n` deep in its own body copies the body per alternative, doubling
/// the peak per step, and on a 128 GiB machine that made a 421-byte program take
/// 2.2 GiB and a 631-byte one 50.8 GB over 20 seconds. The refusal now lands in
/// milliseconds, at exit 3.
fn push_node(nodes: &mut Vec<ProgramNode>, node: ProgramNode) -> Result<ProgramNodeId, EngineCompileError> {
    if nodes.len() >= MAX_LOWERED_NODES {
        return Err(EngineCompileError::Parse(ParseRejection::internal(
            "program exceeded the lowering bound",
        )));
    }
    let id = ProgramNodeId::from_index(nodes.len())
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("program arena exceeds addressing")))?;
    nodes
        .try_reserve(1)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    nodes.push(node);
    Ok(id)
}

/// Classifies one postfix segment into its path step, hoisting any
/// general-expression operand into an [`OperandBinder`] the caller will wrap.
///
/// Frames are appended in the order [`wrap_operand_frames`] walks forward: one per
/// index step, and — once slices join in — `end` before `start`.
fn lower_segment<'ast>(
    segment: &'ast PostfixSegment,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
    binders: &mut Vec<OperandBinder>,
) -> Result<StepAccess, EngineCompileError> {
    match segment {
        PostfixSegment::Field { selector } => lower_field(selector, source, lowerer, binders),
        PostfixSegment::Index { index: Some(index), .. } => lower_index(index, source, lowerer, binders),
        // `.[]` is array/object iteration: an `Each` step. Its optional (`?`)
        // flag comes from the postfix step's own suffix span (handled by the
        // caller), keeping the exact-step law — `.[]?` suppresses only the
        // iterate step's own error.
        PostfixSegment::Index { index: None, .. } => Ok(StepAccess::Each),
        // `.[a:b]`: the AUTHORED bounds ride into the step verbatim. Nothing is
        // normalized here — the wrap keys off the authored sign at runtime, and
        // a non-numeric bound is a legal program whose outcome depends on the
        // input (`null | .["a":2]` is `null`, not a rejection).
        PostfixSegment::Slice { start, end, .. } => {
            // `end` is hoisted FIRST so the forward wrap leaves `start` OUTSIDE it:
            // `start` is generated before `end` within one slice, so `end`'s forks
            // are the younger ones and it varies faster. `[0,1,2,3,4] |
            // [.[(0,1):(3,4)]]` is `[[0,1,2],[0,1,2,3],[1,2],[1,2,3]]`.
            let end = lower_slice_bound(end.as_deref(), source, lowerer, binders)?;
            let start = lower_slice_bound(start.as_deref(), source, lowerer, binders)?;
            Ok(StepAccess::Slice(Box::new(SliceBounds { start, end })))
        }
        PostfixSegment::NodeAccessor { selector } => lower_accessor_segment(selector, false, source, lowerer, binders),
        PostfixSegment::Attribute { selector } => lower_accessor_segment(selector, true, source, lowerer, binders),
        // A standalone suppression segment (`.?`, the second `?` of `.a??`) is
        // the whole-term `try` sugar, split into a catchless `Try` by
        // `lower_postfix_expr` BEFORE this classifier is reached; a stray one here
        // is an internal contract violation.
        PostfixSegment::ErrorSuppression => Err(EngineCompileError::Parse(ParseRejection::internal(
            "error-suppression segment reached lower_segment",
        ))),
    }
}

fn lower_field<'ast>(
    selector: &'ast FieldSelector,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
    binders: &mut Vec<OperandBinder>,
) -> Result<StepAccess, EngineCompileError> {
    match selector {
        FieldSelector::Name(span) => {
            let text = source
                .text()
                .get(span.range())
                .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("field name span out of range")))?;
            Ok(StepAccess::Key(try_copy_str(text).ok_or(
                EngineCompileError::Resource(ResourceError::AllocationFailed),
            )?))
        }
        // `."k"` is a key step; `."k\(1)"` is not — the key is a runtime value,
        // which is the `.[expr]` dynamic index under different punctuation, so it
        // takes the same operand frame.
        FieldSelector::String(template) => {
            if let Some(text) = static_template_text(template, source)? {
                return Ok(StepAccess::Key(text));
            }
            let graph = lower_string_template(template, source, lowerer)?;
            Ok(StepAccess::DynVar(bind_operand_graph(graph, lowerer, binders)?))
        }
        _ => Err(EngineCompileError::unsupported(
            selector.span(),
            UnsupportedConstruct::Expression("an unsupported field selector"),
        )),
    }
}

/// Classifies one `.@`/`.&` selector: static names become
/// [`StepAccess::NodeAccessor`] / [`StepAccess::Attribute`]; a dynamic
/// `@(expr)` / `&(expr)` follows the `.[expr]` operand law.
fn lower_accessor_segment<'ast>(
    selector: &'ast AccessorSelector,
    attribute: bool,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
    binders: &mut Vec<OperandBinder>,
) -> Result<StepAccess, EngineCompileError> {
    if let AccessorSelector::Dynamic { selector, .. } = selector {
        lower_dynamic_accessor(selector, attribute, source, lowerer, binders)
    } else {
        let name = lower_accessor_selector(selector, attribute, source)?;
        Ok(if attribute {
            StepAccess::Attribute(name)
        } else {
            StepAccess::NodeAccessor(name)
        })
    }
}

/// Resolves one bare `$var` OPERAND the way the expression-position arm does,
/// answering the lexical slot when the name has one.
///
/// `Ok(None)` leaves everything else to the caller's general operand
/// lowering: [`bind_operand`] walks the variable through the FULL expression
/// ladder, so a CLI binding (`--arg`/`--argjson`), a data-import alias, and
/// the two named bindings serve `.[$k]` exactly as they serve
/// expression-position `$k` — `$ENV` lowers to the `env/0` read and
/// `$__loc__` folds to its location literal there — and a genuinely unbound
/// name still reports the ordinary undefined-variable error from that same
/// ladder. There is deliberately no operand-position refusal left: refusing
/// `$__loc__` here while expression position accepted it made `.[$__loc__]`
/// diverge from its own `( $__loc__ )` twin, and from jq, which accepts the
/// spelling wherever a term is allowed.
fn resolve_operand_variable(
    span: Span,
    source: &SyntaxSource<'_>,
    lowerer: &Lowerer<'_, '_>,
) -> Result<Option<VarSlot>, EngineCompileError> {
    let Some(name) = source.text().get(span.range()) else {
        return Err(EngineCompileError::Parse(ParseRejection::internal(
            "variable span out of range",
        )));
    };
    if name == "$ENV" || name == "$__loc__" {
        return Ok(None);
    }
    Ok(lowerer.scopes.resolve(name))
}

/// A dynamic accessor selector: a hole-free string folds to the static step,
/// a bare `$var` is the slot itself, and every other expression is hoisted
/// into an operand frame — the same three arms [`lower_index`] / [`lower_field`]
/// use, so a multi-output selector fans out once per string.
fn lower_dynamic_accessor<'ast>(
    selector: &'ast Expr,
    attribute: bool,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
    binders: &mut Vec<OperandBinder>,
) -> Result<StepAccess, EngineCompileError> {
    if let ExprKind::String(template) = selector.kind()
        && let Some(text) = static_template_text(template, source)?
    {
        return Ok(static_accessor_step(&text, attribute));
    }
    if matches!(selector.kind(), ExprKind::Variable)
        && let Some(slot) = resolve_operand_variable(selector.span(), source, lowerer)?
    {
        return Ok(dyn_accessor_step(slot, attribute));
    }
    Ok(dyn_accessor_step(
        bind_operand(selector, source, lowerer, binders)?,
        attribute,
    ))
}

fn static_accessor_step(name: &str, attribute: bool) -> StepAccess {
    let name = normalize_accessor_name(name, attribute);
    if attribute {
        StepAccess::Attribute(name)
    } else {
        StepAccess::NodeAccessor(name)
    }
}

fn dyn_accessor_step(slot: VarSlot, attribute: bool) -> StepAccess {
    if attribute {
        StepAccess::DynAttribute(slot)
    } else {
        StepAccess::DynNodeAccessor(slot)
    }
}

/// The `.@comment_head` alias: a second spelling of the canonical `comment`
/// selector. Applied wherever a selector name is built so nothing downstream
/// sees the alias. The `.&` attribute family is not comment facts.
fn normalize_accessor_name(name: &str, attribute: bool) -> String {
    if !attribute && name == jqf_codec_core::comment::HEAD_ALIAS {
        jqf_codec_core::comment::HEAD.to_owned()
    } else {
        name.to_owned()
    }
}

/// Decodes a STATIC `.@`/`.&` accessor selector into its name string.
///
/// The direct (`expr.@name`) and quoted (`expr.@["name"]`) forms are lowered
/// here — the quoted form's span covers the whole string TOKEN (quotes
/// included), so the literal content is stripped and decoded with the escape
/// set. Write targeting does NOT reach this helper with a computed selector:
/// [`fact_assign_target`] enters through [`lower_accessor_segment`], which
/// routes `AccessorSelector::Dynamic` to [`lower_dynamic_accessor`] BEFORE the
/// static decode, so `PATH.@($role) = RHS` compiles as a dynamic fact write.
/// This helper's own `Dynamic` arm is unreachable from that path and keeps the
/// unsupported-construct rejection for any future caller.
fn lower_accessor_selector<'ast>(
    selector: &'ast AccessorSelector,
    attribute: bool,
    source: &SyntaxSource<'ast>,
) -> Result<String, EngineCompileError> {
    let name = match selector {
        AccessorSelector::Direct { selector } => {
            let text = source.text().get(selector.range()).ok_or_else(|| {
                EngineCompileError::Parse(ParseRejection::internal("accessor selector span out of range"))
            })?;
            try_copy_str(text).ok_or(EngineCompileError::Resource(ResourceError::AllocationFailed))
        }
        AccessorSelector::Bracket { selector, .. } => {
            let mut name = String::new();
            // The selector span covers the quoted token; its literal content is
            // the token with the surrounding quotes stripped, decoded with the
            // escape set (surrogate pairs included). The parser guarantees a
            // plain String token here — an interpolation segment is a parser
            // diagnostic before lowering runs — so the interpolation arm of the
            // decoder is unreachable, and it is mapped to a rejection rather
            // than an internal violation.
            let raw_start = u64::from(selector.start());
            let raw_end = u64::from(selector.end());
            let (start, end) = if raw_end >= raw_start.saturating_add(2) {
                (
                    usize::try_from(raw_start + 1).map_err(|_| {
                        EngineCompileError::Parse(ParseRejection::internal("accessor selector offset overflow"))
                    })?,
                    usize::try_from(raw_end - 1).map_err(|_| {
                        EngineCompileError::Parse(ParseRejection::internal("accessor selector offset overflow"))
                    })?,
                )
            } else {
                return Err(EngineCompileError::Parse(ParseRejection::internal(
                    "empty accessor selector",
                )));
            };
            decode_literal_segment(Span::from_usize(start, end), source, &mut name)?;
            Ok(name)
        }
        // A dynamic selector never arrives here: [`lower_accessor_segment`]
        // routes the dynamic form to [`lower_dynamic_accessor`] first, which is
        // what lets a write admit `.@($role)` as a dynamic fact target. This
        // arm rejects only for a caller that hands a dynamic selector straight
        // to this static decoder.
        AccessorSelector::Dynamic { .. } => Err(EngineCompileError::unsupported(
            selector.selector_span(),
            UnsupportedConstruct::Expression("dynamic accessor selector"),
        )),
    }?;
    Ok(normalize_accessor_name(&name, attribute))
}

/// The one-step vector a `..` stage starts from.
fn descend_steps() -> Result<Vec<StageStep>, EngineCompileError> {
    let mut steps = Vec::new();
    steps
        .try_reserve_exact(1)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    steps.push(StageStep::new(StepAccess::Descend, false));
    Ok(steps)
}

/// Lowers one authored slice bound — a constant stored VERBATIM, anything else
/// hoisted into an operand frame.
///
/// An absent bound is [`SliceBound::Open`]; a scalar literal (including a
/// negated numeric literal and an authored `null`) is kept as the authored
/// [`jqf_data::Value`], because the runtime law keys the len-relative wrap off
/// the authored SIGN and no lower-time integer can carry it. A bare `$var`
/// resolves to its lexical slot, exactly like `.[$i]`.
///
/// Every other bound is a general expression, and it takes the SAME frame an
/// index operand takes ([`bind_operand`]) — which is where the generator law
/// comes from for free: the cartesian product, the start-outer/end-inner order,
/// and an `empty` bound producing zero outputs (`[0,1,2,3,4] | .[empty:2]` emits
/// nothing) are all `Bind`'s own behaviour. The bound TYPE law is untouched: a
/// non-numeric bound is a legal program whose outcome depends on the input
/// (`null | .["a":2]` is `null`; `[0,1,2] | .["a":1]` raises "Array/string slice
/// indices must be integers").
fn lower_slice_bound<'ast>(
    bound: Option<&'ast Expr>,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
    binders: &mut Vec<OperandBinder>,
) -> Result<SliceBound, EngineCompileError> {
    let Some(bound) = bound else {
        return Ok(SliceBound::Open);
    };
    if let Some(literal) = match fold_constant(bound, source, lowerer.resources) {
        Ok(Some(value)) => Some(SliceBound::Literal(value)),
        Ok(None) => None,
        Err(error) => return Err(error),
    } {
        return Ok(literal);
    }
    if matches!(bound.kind(), ExprKind::Variable)
        && let Some(slot) = resolve_operand_variable(bound.span(), source, lowerer)?
    {
        // A bare `$var` whose binding is a lower-time constant folds to the
        // literal, exactly as if the bound had been authored: `1 as $from |
        // .[$from:]` behaves byte-for-byte like `.[1:]` (the binding's own
        // `Bind` frame still runs; the fold only replaces the bound's slot
        // read with the value the binder provably delivers).
        if let Some(value) = constant_binding(lowerer, slot) {
            return Ok(SliceBound::Literal(value.clone()));
        }
        return Ok(SliceBound::Var(slot));
    }
    // A variable that named no lexical slot — a CLI binding (`--arg k 1`),
    // a data-import alias, or an unbound name — lowers through the general
    // operand path: the full expression ladder serves the first two and
    // rejects the last.
    Ok(SliceBound::Var(bind_operand(bound, source, lowerer, binders)?))
}

/// The bound a COMPILE-TIME slice operand folds to, or `None` when the operand
/// needs a runtime frame.
///
/// A computed bound that is a `+`/`-` of
/// provably-constant numbers folds through the SAME exact arithmetic the
/// executor uses ([`compute_number`]), so a computed bound `.[(1+2):]` becomes
/// the literal `.[3:]` and every recognizer and pushdown that keys off an
/// authored literal bound — the range-projection early stop, the count
/// transfers, the `top_k` recognizer — engages without change. Anything the fold
/// cannot prove stays dynamic (a `bind_operand` frame), and a fold never fires
/// a runtime path that would not: the operands are restricted to numbers, which
/// is the one `+`/`-` pair with no mismatch cell and no type-dependent
/// fallback in the executor.
/// One expression's provably-constant VALUE, or `None` when the expression
/// needs a runtime frame.
///
/// This is the lowering fold, distinct from the module-metadata law
/// [`evaluate_constant`]; it widens the authored-literal set with exactly two
/// additions — a
/// `Group` unwrap and a `+`/`-` of constant numbers. The value is produced by
/// the same lowering that produces any authored literal, so the folded
/// `SliceBound::Literal` is indistinguishable from one the user wrote.
fn fold_constant<'ast>(
    expr: &'ast Expr,
    source: &SyntaxSource<'ast>,
    resources: &ResourceContext<'_>,
) -> Result<Option<Value>, EngineCompileError> {
    match expr.kind() {
        ExprKind::Null => Ok(Some(Value::Null)),
        ExprKind::Bool(value) => Ok(Some(Value::Bool(*value))),
        ExprKind::Number => Ok(Some(lower_number(expr.span(), false, source)?)),
        ExprKind::Unary(unary) if unary.op == UnaryOp::Negate && matches!(unary.expr.kind(), ExprKind::Number) => {
            Ok(Some(lower_number(unary.expr.span(), true, source)?))
        }
        ExprKind::String(template) => match static_template_text(template, source)? {
            Some(text) => Ok(Some(literal_string(&text, resources)?)),
            None => Ok(None),
        },
        ExprKind::Group { expression, .. } => fold_constant(expression, source, resources),
        ExprKind::Binary(binary) if matches!(binary.op, BinaryOp::Add | BinaryOp::Subtract) => {
            // The arm IS `fold_constant_number`'s own `+`/`-` arm, wrapped in
            // the authored literal's `Value` shell.
            Ok(fold_constant_number(expr, source, resources)?.map(Value::Number))
        }
        _ => Ok(None),
    }
}

/// The NUMBER one constant operand of a folded `+`/`-` denotes, or `None`.
///
/// Only numbers fold: the executor's `+`/`-` has type-dependent fallbacks
/// (string concat, array concat, object merge, the null additive identity) and
/// the null arm fires a mismatch cell, so folding anything but the numeric pair
/// would skip a runtime path the unfolded program takes. A `Group` unwraps and
/// a nested `+`/`-` recurses, so `((1+2)+3)` folds to `6`.
fn fold_constant_number<'ast>(
    expr: &'ast Expr,
    source: &SyntaxSource<'ast>,
    resources: &ResourceContext<'_>,
) -> Result<Option<Number>, EngineCompileError> {
    match expr.kind() {
        ExprKind::Number => Ok(Some(number_of(lower_number(expr.span(), false, source)?))),
        ExprKind::Unary(unary) if unary.op == UnaryOp::Negate && matches!(unary.expr.kind(), ExprKind::Number) => {
            Ok(Some(number_of(lower_number(unary.expr.span(), true, source)?)))
        }
        ExprKind::Group { expression, .. } => fold_constant_number(expression, source, resources),
        ExprKind::Binary(binary) if matches!(binary.op, BinaryOp::Add | BinaryOp::Subtract) => {
            let Some(left) = fold_constant_number(&binary.left, source, resources)? else {
                return Ok(None);
            };
            let Some(right) = fold_constant_number(&binary.right, source, resources)? else {
                return Ok(None);
            };
            let op = if binary.op == BinaryOp::Add {
                ArithOp::Add
            } else {
                ArithOp::Subtract
            };
            match compute_number(op, &left, &right, resources) {
                Ok(number) => Ok(Some(number)),
                Err(_) => Ok(None),
            }
        }
        _ => Ok(None),
    }
}

/// The [`Number`] a constant [`Value`] is, when the fold restricted it to the
/// numeric pair (`None` is unreachable for a fold-produced value, but the
/// return keeps the caller's match total).
fn number_of(value: Value) -> Number {
    match value {
        Value::Number(number) => number,
        _ => unreachable!("fold_constant_number only produces numbers"),
    }
}

/// The provably-constant value one bound slot holds while its binder's BODY is
/// being lowered, or `None` when the slot is dynamic (or out of the recorded
/// extents). The innermost entry for the slot wins, which is the binder the
/// body's `$var` actually resolves to.
fn constant_binding<'a>(lowerer: &'a Lowerer<'_, '_>, slot: VarSlot) -> Option<&'a Value> {
    lowerer
        .const_bindings
        .iter()
        .rev()
        .find(|(bound_slot, _)| *bound_slot == slot)
        .map(|(_, value)| value)
}

/// Lowers `.[e]` — a constant operand folded into its step, everything else
/// hoisted into an operand frame.
///
/// One runtime law serves every non-constant form, because [`StepAccess::DynVar`]
/// already type-dispatches on the bound value: a string key on an object, a
/// number TRUNCATED toward zero and len-wrapped on an array, `null` on `null`,
/// and the `Cannot index …` refusal otherwise. `.[1.5]` is not a construct of its
/// own — it is `.[e]` with a numeric operand the runtime truncates
/// (`[10,20,30] | .[1.7]` is `20`).
fn lower_index<'ast>(
    index: &'ast Expr,
    source: &SyntaxSource<'ast>,
    lowerer: &mut Lowerer<'ast, '_>,
    binders: &mut Vec<OperandBinder>,
) -> Result<StepAccess, EngineCompileError> {
    if let Some(access) = constant_index(index, source)? {
        return Ok(access);
    }
    // `.[$i]`: a bare bound variable is already a slot, so it needs no frame of
    // its own — which is what keeps `.[$x]` at exactly its pre-vertical cost.
    // A variable naming NO lexical slot (a CLI `--arg` binding, a data-import
    // alias) falls to the general operand path below, which serves it through
    // the same ladder expression-position `$k` uses; `$ENV` lowers to the
    // env/0 read there and `$__loc__` folds to its location literal, so the
    // two named bindings keep one surface across both positions.
    if matches!(index.kind(), ExprKind::Variable)
        && let Some(slot) = resolve_operand_variable(index.span(), source, lowerer)?
    {
        return Ok(StepAccess::DynVar(slot));
    }
    Ok(StepAccess::DynVar(bind_operand(index, source, lowerer, binders)?))
}

/// The step a COMPILE-TIME index operand folds to, or `None` when the operand
/// needs a runtime frame.
///
/// An integer literal (either sign) is a static [`StepAccess::Index`] and a
/// hole-free string is a static [`StepAccess::Key`] — the two spellings every
/// landed pushdown, fusion and projection receipt keys off. A numeric literal
/// that is not an `i64` falls through rather than failing: the runtime's own
/// truncation and len-wrap is the whole law for it.
fn constant_index(index: &Expr, source: &SyntaxSource<'_>) -> Result<Option<StepAccess>, EngineCompileError> {
    match index.kind() {
        ExprKind::Number => parse_signed_index(index.span(), false, source),
        ExprKind::Unary(unary) if unary.op == UnaryOp::Negate && matches!(unary.expr.kind(), ExprKind::Number) => {
            parse_signed_index(unary.expr.span(), true, source)
        }
        ExprKind::String(template) => Ok(static_template_text(template, source)?.map(StepAccess::Key)),
        _ => Ok(None),
    }
}

/// The static index step for an integer literal, or `None` when the digits are
/// not an `i64` (`.[1.5]`, `.[1e999]`, an out-of-range magnitude).
fn parse_signed_index(
    digits_span: Span,
    negative: bool,
    source: &SyntaxSource<'_>,
) -> Result<Option<StepAccess>, EngineCompileError> {
    let text = source
        .text()
        .get(digits_span.range())
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("index span out of range")))?;
    let Ok(magnitude) = text.parse::<i64>() else {
        return Ok(None);
    };
    let value = if negative {
        let Some(negated) = magnitude.checked_neg() else {
            return Ok(None);
        };
        negated
    } else {
        magnitude
    };
    Ok(Some(StepAccess::Index(value)))
}

fn describe_expr_kind(kind: &ExprKind) -> UnsupportedConstruct {
    match kind {
        // Only leftover `lower_expr` arms reach this classifier: Error, a
        // prefix operator that is not unary minus, and a Binary the guarded
        // arms did not take. Every live BinaryOp has a guarded arm; this
        // catch-all is the hole for a new variant. The rest of the inventory
        // is listed so a new form cannot vanish into `_ =>`.
        ExprKind::Binary(_) => UnsupportedConstruct::Expression("a binary operator"),
        ExprKind::Error => UnsupportedConstruct::Expression("a recovered syntax error"),
        ExprKind::Unary(_) => UnsupportedConstruct::Expression("a prefix operator"),
        ExprKind::Definition(_) => UnsupportedConstruct::Expression("a function definition"),
        ExprKind::Call(_) => UnsupportedConstruct::Expression("a function call"),
        ExprKind::Try(_) => UnsupportedConstruct::Expression("a `try`/`catch`"),
        ExprKind::Identity
        | ExprKind::RecursiveDescent
        | ExprKind::Empty
        | ExprKind::Null
        | ExprKind::Bool(_)
        | ExprKind::Number
        | ExprKind::String(_)
        | ExprKind::Variable
        | ExprKind::Format
        | ExprKind::FormatTemplate { .. }
        | ExprKind::Group { .. }
        | ExprKind::Array { .. }
        | ExprKind::Object { .. }
        | ExprKind::Assignment(_)
        | ExprKind::EngineCall { .. }
        | ExprKind::EngineTerm { .. }
        | ExprKind::Postfix(_)
        | ExprKind::If(_)
        | ExprKind::Reduce(_)
        | ExprKind::Foreach(_)
        | ExprKind::Binding(_)
        | ExprKind::Label { .. }
        | ExprKind::Break { .. } => UnsupportedConstruct::Expression("an unsupported expression"),
    }
}

#[cfg(test)]
mod tests {
    use super::{ProgramNode, lower};
    use crate::program::{ProgramNodeId, StageStart, StepAccess};
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
        let (nodes, root, _slots, _engine_slots, _callables, _inputs_cursor, _runtime_index) =
            lower(bound.root(), bound.source(), &test_resources()).expect("lowers");
        (nodes, root)
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
}
