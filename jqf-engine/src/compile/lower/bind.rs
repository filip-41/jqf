//! Patterns, labels, `as` bindings, engine cursors, and `reduce`/`foreach`.
//!
//! Variable resolution stays on [`Scopes`]; this file opens and closes those
//! scopes around bodies.

#[allow(clippy::wildcard_imports)]
use super::*;

pub(crate) fn pattern_variable<'text>(
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
pub(crate) struct PatternBinding {
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
pub(crate) fn collect_pattern_bindings<'ast>(
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
        PatternKind::Error => Err(crate::compile::parse::recovered_syntax_error()),
        PatternKind::EngineBinding => Err(EngineCompileError::unsupported(
            pattern.span(),
            UnsupportedConstruct::Expression("an unsupported binding pattern"),
        )),
    }
}

/// Flattens a right-associative `?//` chain into its alternatives, in source
/// order. A pattern with no `?//` flattens to itself.
pub(crate) fn flatten_alternatives<'ast>(
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
pub(crate) fn collect_pattern_names(
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
pub(crate) fn record_name(name: &str, names: &mut Vec<String>) -> Result<(), EngineCompileError> {
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
pub(crate) fn open_alternative_arm<'ast>(
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
pub(crate) fn chain_alternatives(
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
pub(crate) fn collect_member_bindings<'ast>(
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
pub(crate) fn computed_member(
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
pub(crate) fn record_binding(
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
pub(crate) fn variable_step(
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
pub(crate) fn open_pattern_scope(
    bindings: &[PatternBinding],
    lowerer: &mut Lowerer<'_, '_>,
) -> Result<usize, EngineCompileError> {
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
pub(crate) fn close_pattern_scope(opened: usize, lowerer: &mut Lowerer<'_, '_>) {
    for _ in 0..opened {
        lowerer.scopes.pop();
    }
}

/// Nests a collected frame around `body`, FIRST binder outermost.
///
/// Left-to-right nesting is what makes a computed key's generator the outer loop
/// of every member after it (`{"a":1,"b":2} | [. as {(("a","b")):$x, c:$y} |
/// [$x,$y]]` is `[[1,null],[2,null]]`).
pub(crate) fn wrap_pattern_frame(
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
pub(crate) fn lower_label<'ast>(
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
pub(crate) fn label_name<'source>(
    span: Span,
    source: &'source SyntaxSource<'_>,
) -> Result<&'source str, EngineCompileError> {
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
pub(crate) fn resolve_label(
    span: Span,
    source: &SyntaxSource<'_>,
    labels: &LabelScopes,
) -> Result<LabelSlot, EngineCompileError> {
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
pub(crate) fn lower_binding<'ast>(
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
    let opened = named.len();
    for (name, slot) in &named {
        lowerer.scopes.open(name, *slot)?;
    }
    let body = lower_expr(&binding.body, source, lowerer);
    for _ in 0..opened {
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
pub(crate) fn is_engine_constructor(name: &str) -> bool {
    matches!(name, "generator" | "cursor" | "inputs" | "rng")
}

/// The value-position rejection reason for a constructor reference: the
/// constructor must be the value of an `as ~x` binder, or its cursor could
/// never be pulled.
pub(crate) fn constructor_must_bind(name: &str) -> alloc::string::String {
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
pub(crate) fn lower_engine_binding<'ast>(
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
pub(crate) fn lower_engine_constructor<'ast>(
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
pub(crate) fn alternative_union(
    alternatives: &[&Pattern],
    source: &SyntaxSource<'_>,
) -> Result<Vec<String>, EngineCompileError> {
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
pub(crate) fn lower_loop<'ast>(
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
pub(crate) fn lower_alternative_loop<'ast>(
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
    let opened = named.len();
    for (name, slot) in &named {
        lowerer.scopes.open(name, *slot)?;
    }
    let update = lower_expr(&loop_expr.update, source, lowerer);
    for _ in 0..opened {
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
pub(crate) fn lower_loop_body<'ast>(
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
