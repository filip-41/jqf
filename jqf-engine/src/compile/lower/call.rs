//! Builtin call expansion, assignment, counted streams, and `if`.
//!
//! Registry-resolved calls (`map`, `limit`, `+=`, …) and the assignment /
//! countdown / conditional forms they desugar into.

#[allow(clippy::wildcard_imports)]
use super::*;

/// Lowers one builtin call, resolving `(name, arity)` before lowering arguments.
///
/// Resolution runs against the registry first (AGENTS.md). An unresolved
/// `(name, arity)` is the `name/arity is not defined`. A resolved `Evaluator`
/// (`length`, `keys`, `select`) becomes a [`ProgramNode::Call`] storing the
/// stable overload id and its separate semantic revision; a resolved `Lowering`
/// (`map`) expands into the arena instead of ever becoming a `Call`.
pub(crate) fn lower_call<'ast>(
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
pub(crate) fn lower_map<'ast>(
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
pub(crate) fn lower_paths<'ast>(
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
pub(crate) fn lower_del<'ast>(
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
pub(crate) fn lower_with_entries<'ast>(
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
pub(crate) fn lower_add<'ast>(
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
pub(crate) fn lower_in<'ast>(
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
pub(crate) fn lower_pick<'ast>(
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
pub(crate) fn lower_in_stream<'ast>(
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
pub(crate) fn lower_indexed_fold<'ast>(
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
pub(crate) fn lower_join_indexed<'ast>(
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
pub(crate) fn empty_object_stage(lowerer: &mut Lowerer<'_, '_>) -> Result<ProgramNodeId, EngineCompileError> {
    let object = jqf_data::ObjectBuilder::try_with_capacity(0)
        .and_then(jqf_data::ObjectBuilder::try_finish)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    push_node(&mut lowerer.nodes, literal_stage(Value::Object(object)))
}

/// `[.[] | body]` — the array-collecting fan-out `map` and `with_entries` share.
pub(crate) fn collect_each(
    body: ProgramNodeId,
    lowerer: &mut Lowerer<'_, '_>,
) -> Result<ProgramNodeId, EngineCompileError> {
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
pub(crate) fn builtin_call(
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
pub(crate) fn lower_first<'ast>(
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
pub(crate) fn cap_at_first_output(
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
pub(crate) fn lower_assignment<'ast>(
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
pub(crate) enum FactSelector {
    /// A static selector: the role text and the fact kind (the attribute
    /// name for a `.&name` write, empty for the node-fact roles).
    Static { role: String, kind: String },
    /// A dynamic selector (`PATH.@(expr)` / `PATH.&(expr)`): the operand is
    /// bound outside the write and resolved at run time; `attribute` marks
    /// the `.&` form.
    Dynamic { slot: VarSlot, attribute: bool },
}

impl FactSelector {
    pub(crate) fn role(&self) -> String {
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

    pub(crate) fn kind(&self) -> String {
        match self {
            Self::Static { kind, .. } => kind.clone(),
            Self::Dynamic { .. } => String::new(),
        }
    }

    /// The node field's dynamic-selector slot: `None` for a static write.
    pub(crate) fn selector(&self) -> Option<(VarSlot, bool)> {
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
pub(crate) struct FactTarget<'ast> {
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
pub(crate) fn fact_assign_target<'ast>(
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

/// Lowers one FACT assignment into a [`ProgramNode::FactAssign`],
/// mirroring [`lower_assignment`]'s operator law exactly: `=` binds its
/// right-hand side OUTSIDE the node, `|=` keeps the update as a filter, and
/// `op=`/`//=` compose the current payload with the bound value.
#[allow(
    clippy::too_many_lines,
    reason = "one lowering per operator family: the fact-assignment drive mirrors the \
              value-assignment drive's binding law arm for arm"
)]
pub(crate) fn lower_fact_assignment<'ast>(
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
pub(crate) fn push_fact_assign(
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
pub(crate) fn lower_fact_path<'ast>(
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
pub(crate) fn is_key_index_chain(nodes: &[ProgramNode], id: ProgramNodeId) -> bool {
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
pub(crate) fn modify_node(
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
pub(crate) const fn arithmetic_update_kind(op: AssignmentOp) -> Option<BinaryKind> {
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
pub(crate) fn lower_limit<'ast>(
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
pub(crate) const LIMIT_NEGATIVE: &str = "limit doesn't support negative count";
/// The refusal for a negative `skip` count.
pub(crate) const SKIP_NEGATIVE: &str = "skip doesn't support negative count";
/// The refusal for a negative `nth` index — a DIFFERENT noun from the other
/// two, and the reason the message is a parameter rather than a constant.
pub(crate) const NTH_NEGATIVE: &str = "nth doesn't support negative indices";

/// Lowers `nth($n)` into its definition: `.[$n]`.
///
/// The index is a BOUND value, so a generator index fans out (`nth(0,2)` reads
/// two positions) and a negative one wraps exactly as `.[-1]` does — which is
/// why this arity accepts what `nth/2` refuses.
pub(crate) fn lower_nth_index<'ast>(
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
pub(crate) fn lower_nth<'ast>(
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
pub(crate) fn lower_skip<'ast>(
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
pub(crate) fn lower_counted_stream<'ast>(
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
pub(crate) fn integer_stage(value: i64) -> ProgramNode {
    ProgramNode::Stage {
        start: StageStart::Literal(Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(
            value,
        )))),
        steps: Vec::new(),
    }
}

/// `$slot <op> 0`.
pub(crate) fn var_compared_to_zero(
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
pub(crate) fn negative_count_error(text: &str, lowerer: &mut Lowerer) -> Result<ProgramNodeId, EngineCompileError> {
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
pub(crate) fn lower_inside<'ast>(
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
pub(crate) fn each_steps() -> Result<Vec<StageStep>, EngineCompileError> {
    let mut steps = Vec::new();
    steps
        .try_reserve_exact(1)
        .map_err(|_| EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    steps.push(StageStep::new(StepAccess::Each, false));
    Ok(steps)
}

/// Maps a parser binary operator to this vertical's [`BinaryKind`], or `None` for
/// an operator another vertical owns (pipe, comma, `and`/`or`/`//`).
pub(crate) fn arithmetic_binary_kind(op: BinaryOp) -> Option<BinaryKind> {
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
pub(crate) fn logical_operator(op: BinaryOp) -> Option<LogicalOp> {
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
pub(crate) fn lower_conditional<'ast>(
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
