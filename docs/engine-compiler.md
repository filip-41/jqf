# Engine compiler

Compile is how jq source becomes a plan the rest of the request can trust. There
is no bytecode and no stack VM. The compiler lowers the program to a
[generator graph](engine-ir.md), fuses it into path-normal form, walks the
closed [recognizer tables](recognizers.md), and caches the facts a drive will
consult. This page is the pipeline under
[Architecture § Engine IR](architecture.md#engine-ir). Execution, codec access,
and the recognizers themselves live on their own pages.

A form is not in the language until this pipeline accepts it. Syntax may parse a
construct compile still rejects.

## What comes out

Embedders and the CLI call one entry,
[`try_compile_program`](embedding.md#compile). The result is an opaque
`CompiledProgram`: the arena, the
[pushdown split](engine-ir.md#the-pushdown-split), and the route facts
`--explain` prints. `try_requirement` and `try_run` are the observations a
drive makes; it does not re-walk the arena per record.

```console
$ echo '{"users":[{"name":"a"}]}' | jqf --explain '.users[].name' 2>&1 | grep -E 'program:|class:|demand:|routes:|pushdown:|ladder:|topk:|compile_time:|lazy:|^"'
jqf: explain: program: .users[].name
jqf: explain: class: identity=no modifies=no whole_document=yes input_family=no morsel_static=no
jqf: explain: demand: class=Fields(name) boundary=residual
jqf: explain: routes: count=no element=yes keys=no type=no inputs_cursor=no
jqf: explain: pushdown: .users
jqf: explain: ladder: morsel=yes range_locate=no
jqf: explain: topk: rows=0
jqf: explain: compile_time: 2.28ms
"a"
jqf: explain: lazy: deferred=1 materialized=0
```

`.users` is the prefix the codec is asked to resolve. `Fields(name)` is how much
of each streamed element the residual reads. `deferred=1` is the receipt that
the unread sibling fields stayed spans. The same accessors feed
[plan pinning](explain.md#plan-pinning).

## Stage order

Gate through transform can fail. Analyze and finish do not surface compile
errors; they record facts on an arena that already lowered.

```text
user source
    │
    ▼
prelude gate     stdlib / extension text, parsed once per process
    │
    ▼
parse ──► bind ──► lower ──► transform ──► analyze ──► finish
                       │          │             │           │
                  generator   fuse + marks   pushdown    cached
                  arena                                 route facts
```

`--arg` / `--argjson` values are compiled in as literals. A binder in the
program always beats a CLI binding. Later CLI entries shadow earlier ones.
`CompileOptions::new()` is the ordinary lane; a split-expression compile
(`CompileOptions::split_exp()`, `$index` pre-bound for `--split-exp` /
`--split-exp-file`) is the same pipeline with a seeded slot and no CLI
bindings.

## Preludes

`any/2` is an ordinary `def` in the stdlib prelude (`isempty`, `all`, `any`,
`first`, `last`, `values`, `nulls`), not a second namespace. `map/1` is a
builtin — `[.[] | f]` — not a prelude name. Before parse, compile scans the
user source once:

- `import` / `include` / `module` pull in both the stdlib and extension
  preludes.
- Otherwise, identifier tokens are matched against the prelude name lists.
  `\(…)` interpolation holes are code and are scanned; only string text and
  `#` comments are skipped.

A false *hit* only wastes a prelude parse. A false *miss* is unsound: a real
prelude call would report "not defined". When a prelude is needed it is parsed
and bound once per process and reused. Those definitions sit on the `def` stack
before the user unit lowers, so a prelude name resolves like any other call.

```console
$ echo '[]' | jqf --explain 'map(.+1)' 2>&1 | grep -E 'program:|demand:|routes:|pushdown:'
jqf: explain: program: map(.+1)
jqf: explain: demand: class=Subtree boundary=collect
jqf: explain: routes: count=no element=no keys=no type=no inputs_cursor=no
jqf: explain: pushdown: .
```

`map/1` is a builtin. The user source never spelled the body; lowering
expands it to `[.[] | f]`.

## Parse, bind, modules

Parse is `jqf-syntax`. Recoverable debris is rejected before bind — no
lowering on a broken tree. Bind attaches span text for diagnostics,
`$__loc__`, and module paths.

`include` / `import` resolve at compile through the host module loader
(CLI search path, authored `{search: …}` metadata, or
`JQF_LIBRARY_PATH`). A missing loader or an unresolved path is "module
not found". An empty search list is not a silent skip. Circular imports
fail at compile. Filter-parameter defs (`def f(map):
…`) re-lower at each call site so a parameter name can shadow a builtin
spelling without changing call-by-name at runtime.

## Lowering

The bound tree becomes the dense arena [Engine IR](engine-ir.md) describes.
Non-recursive `def` bodies inline at call sites. Recursion becomes an explicit
callable the executor invokes. `[body] | length` is Engine IR's fused
`CountCollect`.

Some names are catalogue reads, not shadowable variables — `$ENV` is the
environment object even under `1 as $ENV | $ENV`. Lowering also records whether
the program binds the `~inputs` cursor, and the `$index` slot when the
split-expression lane is on.

## Transform

The pre-fusion arena is rewritten in a fixed order. One structural rewrite
rides alongside fusion: `[stream] | add` at the program root (and inside
deduped callable bodies) becomes
`reduce stream as $x (null; . + $x)`. Marks (tail calls, keyed collects, static
object keys) run after fusion, because topology has to be final.

The arena at rest is in **path-normal form**. That law, and the pushdown split
taken from it, live on [Engine IR](engine-ir.md#fusion-and-path-normal-form).

## Analyze and finish

Analysis walks the fused arena once and takes the
[pushdown split](engine-ir.md#the-pushdown-split) and the projection class
([Demand and pushdown](demand.md), [Shape recognizers](recognizers.md)).
`finish` then caches everything a per-record drive will ask: prune and
pulled-record hints, count, element / construct / collect, `keys`, and
`type`. The projection class is computed once there and shared with the
count table. A drive that re-derived those facts per record would be a
contract break.

Dead nodes in the arena do not force a scan pass. A shape the closed tables
decline takes the ordinary executor floor, byte for byte — see
[Shape recognizers](recognizers.md).

## When compile refuses

Name errors, parse failures, constructs outside the landed subset, missing or
circular modules, and a ledger that cannot charge the arena all fail here,
before a drive starts:

```console
$ echo '{}' | jqf 'nosuchfn(1)'
jqf: nosuchfn/1 is not defined at bytes 0..8
  nosuchfn(1)
  ^^^^^^^^
```

Under [`--explain`](explain.md), a parse-class error may still print a recovered
tree outline before the ordinary compile line.

## Where to read next

| Topic                                     | Page                                  |
| ----------------------------------------- | ------------------------------------- |
| Node kinds, fusion, pushdown              | [Engine IR](engine-ir.md)             |
| Closed tables and `--explain` shape lines | [Shape recognizers](recognizers.md)   |
| Access requirements and routes            | [Demand and pushdown](demand.md)      |
| Plan output                               | [Explain and diagnostics](explain.md) |
| SDK compile + run                         | [Embedding jqf](embedding.md)         |
