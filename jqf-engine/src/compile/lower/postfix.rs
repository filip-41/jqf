//! Literals, templates, object members, postfix steps, and constant folds.
//!
//! Turn `.a[i]?`, `{k: v}`, and string templates into arena nodes, hoisting
//! general-expression operands into [`OperandBinder`] frames.

#[allow(clippy::wildcard_imports)]
use super::*;

/// A scalar-literal producer: a [`StageStart::Literal`] stage with no steps.
pub(crate) fn literal_stage(value: Value) -> ProgramNode {
    ProgramNode::Stage {
        start: StageStart::Literal(value),
        steps: Vec::new(),
    }
}

/// Whether an expression is `.` however it was spelled — bare, parenthesised,
/// or piped into itself. Only these three: anything that could publish a
/// different value, or publish it a different number of times, is not the unit.
pub(crate) fn is_identity(expr: &Expr) -> bool {
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
pub(crate) const fn current_stage() -> ProgramNode {
    ProgramNode::Stage {
        start: StageStart::Current,
        steps: Vec::new(),
    }
}

/// A bound-variable producer: a [`StageStart::Variable`] stage with no steps yet
/// (a postfix chain on `$x` fuses its steps onto this same stage).
pub(crate) const fn variable_stage(slot: VarSlot) -> ProgramNode {
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
pub(crate) fn lower_string_template<'ast>(
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
pub(crate) fn push_concat(
    parts: Vec<ProgramNodeId>,
    lowerer: &mut Lowerer<'_, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
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
pub(crate) fn lower_template_segment<'ast>(
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
pub(crate) fn lower_format(
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
pub(crate) fn lower_format_template<'ast>(
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
pub(crate) fn format_name(
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
pub(crate) fn literal_string(text: &str, _resources: &ResourceContext<'_>) -> Result<Value, EngineCompileError> {
    Value::try_string(text).map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))
}

/// Applies the `_negate` value law to an already-parsed numeric literal, so a
/// folded `-<number>` term and the runtime operator cannot disagree.
pub(crate) fn negated_literal(magnitude: Value) -> Result<Value, EngineCompileError> {
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
pub(crate) fn lower_object_member<'ast>(
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
pub(crate) fn lower_variable_key_member<'ast>(
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
        let data = lowerer.module_vars[position].1.clone();
        let (key, value) = if let Some(expr) = value {
            // `{$d: e}` — the module binding is the KEY producer, not a
            // static stripped identifier.
            let key = push_node(&mut lowerer.nodes, literal_stage(data.clone()))?;
            let value = lower_expr(expr, source, lowerer)?;
            (key, value)
        } else {
            let name = variable_key_text(span, source)?;
            let key_literal = literal_string(name, lowerer.resources)?;
            let key = push_node(&mut lowerer.nodes, literal_stage(key_literal))?;
            let value = push_node(&mut lowerer.nodes, literal_stage(data))?;
            (key, value)
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
pub(crate) fn variable_key_text<'text>(
    span: Span,
    source: &'text SyntaxSource<'_>,
) -> Result<&'text str, EngineCompileError> {
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
pub(crate) fn lower_interpolated_key_member<'ast>(
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
pub(crate) fn lower_bound_key_lookup(
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
pub(crate) fn single_dyn_step(slot: VarSlot) -> Result<Vec<StageStep>, EngineCompileError> {
    let mut steps = Vec::new();
    steps
        .try_reserve_exact(1)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    steps.push(StageStep::new(StepAccess::DynVar(slot), false));
    Ok(steps)
}

/// Lowers a static-keyed member: a singleton literal-string key producer, plus
/// either the explicit value graph or the shorthand `.<key>` path.
pub(crate) fn lower_static_key_member<'ast>(
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
pub(crate) fn single_key_step(name: String) -> Result<Vec<StageStep>, EngineCompileError> {
    let mut steps = Vec::new();
    steps
        .try_reserve_exact(1)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    steps.push(StageStep::new(StepAccess::Key(name), false));
    Ok(steps)
}

/// The identifier text at `span` copied into an owned [`String`].
pub(crate) fn identifier_text(span: Span, source: &SyntaxSource<'_>) -> Result<String, EngineCompileError> {
    let text = source
        .text()
        .get(span.range())
        .ok_or_else(|| EngineCompileError::Parse(ParseRejection::internal("object key name span out of range")))?;
    copy_string(text)
}

/// Copies `text` into an owned [`String`] with a fallible reservation.
pub(crate) fn copy_string(text: &str) -> Result<String, EngineCompileError> {
    try_copy_str(text).ok_or(EngineCompileError::Resource(ResourceError::AllocationFailed))
}

/// Lowers a postfix chain, composing its steps onto its base and splitting each
/// term-level error-suppression `?` into a catchless `try` barrier.
///
/// The base is ANY expression — identity (`.a`), a group (`(.a, .b).c`), a scalar
/// literal (`3[]?`), a constructor (`{a: .b}.a`), a call, a conditional
/// (`if true then [.] else . end []`), a fold (`(reduce .[] as $x (0; .))[0]`) —
/// because a base carries no law of its own — identity, groups, literals,
/// constructors, conditionals, and folds all compose the same way. A
/// [`PostfixSegment::ErrorSuppression`]
/// segment (`(.a)?`, the second `?` of `.a??`, `.?`) is the whole-term `try`
/// SUGAR: its PREFIX (base + steps so far) lowers INTO the `try` body and
/// its SUFFIX composes on the `try`'s outputs OUTSIDE the barrier (`(.a)?.b` and
/// `.a??.b` both error at `.b`).
pub(crate) fn lower_postfix_expr<'ast>(
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
pub(crate) fn find_accessor_step(expr: &Expr) -> Option<Span> {
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
pub(crate) fn lower_engine_pull<'ast>(
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
pub(crate) struct OperandBinder {
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
pub(crate) fn bind_operand<'ast>(
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
pub(crate) fn bind_operand_graph(
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
pub(crate) fn wrap_operand_frames(
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
pub(crate) fn compose_postfix(
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
pub(crate) fn push_node(nodes: &mut Vec<ProgramNode>, node: ProgramNode) -> Result<ProgramNodeId, EngineCompileError> {
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
pub(crate) fn lower_segment<'ast>(
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

pub(crate) fn lower_field<'ast>(
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
pub(crate) fn lower_accessor_segment<'ast>(
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
/// diverge from its own `( $__loc__ )` twin; operand position accepts the
/// spelling wherever a term is allowed.
pub(crate) fn resolve_operand_variable(
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
pub(crate) fn lower_dynamic_accessor<'ast>(
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

pub(crate) fn static_accessor_step(name: &str, attribute: bool) -> StepAccess {
    let name = normalize_accessor_name(name, attribute);
    if attribute {
        StepAccess::Attribute(name)
    } else {
        StepAccess::NodeAccessor(name)
    }
}

pub(crate) fn dyn_accessor_step(slot: VarSlot, attribute: bool) -> StepAccess {
    if attribute {
        StepAccess::DynAttribute(slot)
    } else {
        StepAccess::DynNodeAccessor(slot)
    }
}

/// The `.@comment_head` alias: a second spelling of the canonical `comment`
/// selector. Applied wherever a selector name is built so nothing downstream
/// sees the alias. The `.&` attribute family is not comment facts.
pub(crate) fn normalize_accessor_name(name: &str, attribute: bool) -> String {
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
pub(crate) fn lower_accessor_selector<'ast>(
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
pub(crate) fn descend_steps() -> Result<Vec<StageStep>, EngineCompileError> {
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
pub(crate) fn lower_slice_bound<'ast>(
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
pub(crate) fn fold_constant<'ast>(
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
pub(crate) fn fold_constant_number<'ast>(
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
pub(crate) fn number_of(value: Value) -> Number {
    match value {
        Value::Number(number) => number,
        _ => unreachable!("fold_constant_number only produces numbers"),
    }
}

/// The provably-constant value one bound slot holds while its binder's BODY is
/// being lowered, or `None` when the slot is dynamic (or out of the recorded
/// extents). The innermost entry for the slot wins, which is the binder the
/// body's `$var` actually resolves to.
pub(crate) fn constant_binding<'a>(lowerer: &'a Lowerer<'_, '_>, slot: VarSlot) -> Option<&'a Value> {
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
pub(crate) fn lower_index<'ast>(
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
pub(crate) fn constant_index(
    index: &Expr,
    source: &SyntaxSource<'_>,
) -> Result<Option<StepAccess>, EngineCompileError> {
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
pub(crate) fn parse_signed_index(
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

pub(crate) fn describe_expr_kind(kind: &ExprKind) -> UnsupportedConstruct {
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
