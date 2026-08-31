# Engine executor

Compile commits *one job*. The engine runs that job against the document the
codec decoded. If the document cannot answer, it runs the jq graph. This page is
the execute entry under [Architecture § Engine IR](architecture.md#engine-ir).
Compile itself is [Engine compiler](engine-compiler.md); the arena shape is
[Engine IR](engine-ir.md).

There is no bytecode and no stack VM. The four jobs:

1. **Run the graph** — `StageMachine` / `GraphMachine`. Ordinary jq.
2. **Ask the document** — how many, what keys, what type, stream these fields,
   `has`, `any`/`all`, `min`/`max`. No graph.
3. **Cut a byte range** — `.[1:3]` from source spans. The host owns the codec
   span cut.
4. **Pass through** — identity. The host may reprint the file.

(3) and (4) stay host I/O. Identity echo needs the source window execute does
not hold. Range-locate needs the codec session and byte window.

## What comes in

Embedders compile once, then the SDK loop is decode → execute → encode.
`CompiledProgram::execute` takes the codec's access outcome and returns an
`EngineRun` to poll.

Finish packed one job, not four parallel stories:

```text
program   Program          IR + split + scan/topk/anti. The floor.
plan      AccessPlan       Whole | Exact, prune, count/element/type/has/keys/minmax
shortcut  enum             execute-private oracles; none means graph only
host_io   Run | Echo | SpanCut
run       Residual | Whole how the graph runs if we get there
```

`try_requirement` charges `plan`. It does not re-walk `count_demand()`.
`Err` aborts; it does not fall back to Whole. Range-locate is Exact path +
host SpanCut, not a third footprint. `host_io` is what the SDK matches: Echo
may reprint retained bytes, SpanCut may cut a codec span, Run is decode →
execute → encode. Shortcut stays inside execute. `--explain` still prints
the shortcut tag (plan v10):

```console
$ echo '{"users":[{"name":"a"}]}' | jqf --explain '.users[].name' 2>&1 | grep -E 'program:|class:|demand:|shortcut:|pushdown:|ladder:|topk:|compile_time:|lazy:|^"'
jqf: explain: program: .users[].name
jqf: explain: class: identity=no modifies=no whole_document=yes input_family=no morsel_static=no
jqf: explain: demand: class=Fields(name) boundary=residual
jqf: explain: shortcut: element inputs_cursor=no
jqf: explain: pushdown: .users
jqf: explain: ladder: morsel=yes range_locate=no
jqf: explain: topk: rows=0
jqf: explain: compile_time: 2.28ms
"a"
jqf: explain: lazy: deferred=1 materialized=0
```

The same accessors feed [plan pinning](explain.md#plan-pinning). The
serializable plan is version 10: the shortcut tag; identity and `range_locate`
bools are omitted (the tag already carries both). The decoder still reads v9
(those bools plus the tag), v8's four route bools, and v7, which omitted them.

## Shortcut, then the graph

`execute` matches the committed shortcut on a located access result. Lenient
only. A hit returns a value stream without `StageMachine` / `GraphMachine`. A
miss — the document cannot prove the count, the keys, the probe is not a number
— falls through to the graph. Decline is byte-identical to never having tried
the oracle.

Identity and range-locate fall through here on purpose. `host_io` already named
Echo or SpanCut; execute does not pretend it holds that window.

A shortcut Exact-locates when the packed split has a prefix (pipe form
`.users | all(.id)`). A generator-path that still names the container in
`demand.path` but is a whole-document split stays Whole — extra-read Exact
does not fuse. Empty prefix stays Whole (it really is the document).
Document codecs bind Direct Exact. A clause that slot cannot serve
(attribute / markup) opens Whole via CompleteDocumentExact. The engine does
not special-case the format.

When an Exact shortcut declines, the graph may still need siblings the located
node does not have. JSON Exact republishes the child as the document root
(`node == root`). YAML and HTML native Exact also publish a subtree whose root
is the selection, so that check cannot tell Exact from Whole — execute returns
`ReboundWhole` and the host decodes Whole. CompleteDocumentExact fallback keeps
the full graph and names the child (`node` is not the root): relocate to the
root and run Whole. Count and element Exact miss must not run the residual on
the Exact node when skip (`prefix_len = 0`) is not `demand.path`. Count and
element visit go through `Document::count_children_from` /
`visit_elements_from`.

Access misses that are not shortcut decline:

| Codec outcome                     | Engine run                           |
| --------------------------------- | ------------------------------------ |
| `Missing` or mismatch-on-null     | stream over owned `null`             |
| `TypeMismatch` + `?` on that step | `Suppressed`                         |
| `TypeMismatch` without `?`        | `Pushdown` (typed error, no machine) |

## Residual graph

Two machines, still dictated by shape:

- A bare-`Stage` residual takes the **single-slot fast path**.
- Anything else runs the **graph interpreter** on a unified frame stack.

Call dispatch is one table: a resolved builtin `Call` maps onto the family that
owns that evaluator (binary, user-call, keyed, builtins, generate, modify,
join). Path-family builtins hand off to the path register. One value is in
flight per frame.

`-n` / `-s` keep `try_run_whole_value` because their input is synthesized, never
Exact-located.

## SDK

The host matches `host_io` once, binds the codec with the packed requirement,
calls `execute`, and encodes the value stream. It does not if-ladder count /
keys / element. JSON Exact count or element miss returns `ReboundWhole`; the
host rebinds Whole from the packed Exact+count/element plan, not a second demand
walk. Edit, morsel, and NDJSON vs one file are "how many times do we run the
job," not the job itself.

## Where to read next

| Topic                        | Page                                                  |
| ---------------------------- | ----------------------------------------------------- |
| Gate through finish          | [Engine compiler](engine-compiler.md)                 |
| Node kinds, fusion, pushdown | [Engine IR](engine-ir.md)                             |
| Closed tables                | [Shape recognizers](recognizers.md)                   |
| Access requirements          | [Demand and pushdown](demand.md)                      |
| Plan output                  | [Explain and diagnostics](explain.md)                 |
| SDK compile + run            | [Embedding jqf](embedding.md)                         |
