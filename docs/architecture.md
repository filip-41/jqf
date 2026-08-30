# Architecture

jqf runs jq 1.8.2 programs against a format-neutral document. Codecs own each format's grammar and facts. The engine compiles a generator IR, recognizes a closed set of program shapes, and picks a route. `--edit` patches the original bytes. `--max-rss` caps the process resident set.

## Structure

```text
jqf-cli                 argv, host I/O, RSS, serve
jqf-sdk                 portable execute; picks the drive
jqf-engine              compile, analyze, interpret the residual
jqf-builtins            overload registry and evaluators
jqf-codec-core          access, demand, encode, record streams — no parser
jqf-codec-*             one crate per format; engine sees only core
jqf-data                Document and Value
jqf-syntax              parse only
jqf-source              spans, source identity, diagnostics
jqf-resource            request ledger, work meter, cancel
jqf-runtime             the only crate that owns threads
```

A request walks that stack once. Content is never sniffed: a named file's format follows its extension, stdin is JSON, `--input-format` / `--output-format` always win.

1. The CLI parses argv and opens host I/O.
2. `jqf-syntax` parses the program. A form is not in the language until the compiler accepts it.
3. `jqf-engine` lowers the tree to a generator IR, fuses it, and runs the shape recognizers.
4. The matching codec decodes into a `Document`. Lazy whole-document is the floor: every byte is validated before any node is published. Demand is a hint that lets a route do less work, never fewer guarantees.
5. The engine runs the residual graph. Record streams (NDJSON, CSV, json-seq) are framers over byte ranges, not documents. Adjacent JSON texts are the default stdin; NDJSON is never inferred.
6. Encode projects back to the output format. `--edit` splices assignments into the retained source instead of re-rendering the file.

[`--explain`](explain.md) prints the plan through the same accessors the route selector reads.

## Document

`jqf-data` holds two things, and they are not the same.

**`Value`** is the owned semantic value a jq program sees: null, bool, number, string, bytes, dates and times, array, object, plus an optional tag wrapper. Numbers are integer, exact decimal, or binary64. Arrays keep order. Objects keep unique keys in first-insertion order. `Value` has no `Eq`/`Ord` — the caller that compares owns what equal means.

**`Document`** is one immutable format-neutral document. A codec builds it; nothing mutates it. Nodes, occurrences, and facts are dense ids local to that document. Occurrence topology may keep duplicate keys and shared or cyclic edges (YAML aliases). Semantic object projection still keeps the first key position and the last value.

Facts (`.@comment`, `.@tag`, `.&href`, …) are ordered portable metadata on a node. They cannot change intrinsic meaning. Any operation that constructs a new `Value` drops them — they are provenance, not data.

The lazy document keeps source spans. A codec that can defer materialization answers counts, element streams, kind-only `type`, and `keys` from the span skeleton; a field the program never reads is still *validated*. A corrupt byte in an unread field fails the request the same way the whole-document floor would.

`--edit` is a span patch against that retained source. Untouched bytes stay the file you wrote.

The detail is [Document model](document-model.md).

## Engine IR

jq is a generator language. The compiler does not emit bytecode or a stack VM. It lowers the program to a **generator graph** and interprets that graph.

Lowering emits:

| Node | jq |
| --- | --- |
| `Stage` | a static path: field/index/`.[]` steps, starting from `.` or a literal |
| `FlatMap` | pipe, and group-postfix composition |
| `Choice` | comma |
| `CollectArray` / `ConstructObject` | `[…]` / `{…}` |

Analysis then fuses every `FlatMap(Stage, Stage)` into one `Stage` — `.a | .b` becomes one path. A `Choice`, a constructor, or a literal-start body blocks fusion. The stored arena is in **path-normal form**: no fusable pipe of stages remains.

What is left is a graph of those five node kinds. A bare-`Stage` root (pure path / iteration / literal) is the fast path. A `Choice` or constructor root is a top-level comma or `[…]`/`{…}`. A `FlatMap` root remains only when one side blocked fusion.

The **pushdown split** names the maximal static prefix of the entry stage before its first `.[]`. That prefix becomes a codec `AccessRequirement`; everything from the first iteration onward is the residual the executor drives. Identity is whole-document access, never an empty forward path.

A bare-`Stage` residual takes a single-slot fast path. `Choice` / `FlatMap` engage the graph interpreter. One value is in flight per frame; an unsuppressed failure discards the frame stack.

The detail is [Engine IR](engine-ir.md).

## Shape recognizers

After fusion, analysis walks the arena against **closed tables**. A shape that is not a row takes the ordinary floor, byte for byte. A new shape joins by adding a row, never by widening a default. Recognizers change how the executor walks or what the document-core consumer answers; they do not change published bytes.

| Table | Recognizes | Instead of |
| --- | --- | --- |
| Projection | how much of each streamed element is consumed: `Structure < Fields(S) < Subtree` | always building the whole element |
| Count | `PATH \| length`, `[C[] \| probe] \| length` | walking every element to count |
| Type | `type`, `PATH \| type` | building the named node's payload |
| Element | `.catalog[] \| .name`, collected fan-out including `map`, `reduce` object-increment, `limit`/`first`/`nth`, select fan-out | materializing every element into the evaluator |
| Keys | `keys`, `PATH \| keys` | walking the container to collect names |
| Correlated scan | `.users[] \| select(.id == $o.user_id)` and the `map` spelling | Θ(k·m) nested rescans |
| Partial sort | `sort \| .[0:k]`, `sort_by(f) \| .[-k:]`, `sort \| first`/`last` | a full sort for a k-element question |
| Range locate | a static slice the codec can serve by span | decoding the whole container |

Count, element, type, and keys demands are derived once at compile and consulted per record. The SDK drive publishes the consumer's answer when the document can prove it, and the ordinary route stands when it cannot. Join and partial-sort change no route and no codec requirement — only how many children the executor visits, or whether a bounded heap replaces `sort`.

`--explain` is the public view of this:

```console
$ echo '{"users":[{"name":"a"}]}' | jqf --explain '.users[].name'
jqf: explain: program: .users[].name
jqf: explain: class: identity=no modifies=no whole_document=yes input_family=no morsel_static=no
jqf: explain: demand: class=Fields(name) boundary=residual
jqf: explain: pushdown: .users
jqf: explain: ladder: morsel=yes range_locate=no
jqf: explain: route: stream
"a"
```

| Line | Meaning |
| --- | --- |
| `class:` | program shape — identity, assignment, whole-document, `inputs`, static per-record path |
| `demand:` | `Subtree` or `Fields(...)`, and the consumer (`none`, `residual`, `collect`) |
| `pushdown:` | static path prefix the codec can serve without building the rest |
| `route:` | the route that served the request (`stream`, `record`, `edit`, …) |
| `cost:` | ledger peak, input/output bytes, spill |

`--diagnostics` adds build provenance, the cost snapshot, and the RSS line.

The detail is [Shape recognizers](recognizers.md).

## Codec demand

The recognizers name what the program needs. The codec decides how much of the document to *build*. Engine and selector never branch on which format honoured the hint: they see only `jqf-codec-core`.

Bind is first-match over the routes a codec advertises:

| Footprint | What it serves |
| --- | --- |
| `Exact` | one static path (the pushdown prefix) |
| `Whole` | the complete document |

An exact demand with no Exact slot still opens Whole. Delivering more than asked is sound. Result authority is a hint and may fall back to the lazy whole document. A demand clause no route can serve is a hard mismatch at bind, not a silent repair.

The validating scan visits every byte before any node is published. Deferral changes whether a node is built, not whether a byte is validated. A codec that cannot defer (YAML aliases and merge keys, HTML recovery) says so in its own module; the engine still sees a document.

Record streams (NDJSON, CSV, json-seq, cbor-seq) are a different kind: framers over byte ranges, never documents. Each record's payload then goes through the payload codec's ordinary access ladder — the same Whole / Exact bind, reused, not a second decode stack.

The detail is [Demand and pushdown](demand.md).

## Crates

| Crate | Job |
| --- | --- |
| `jqf-source` | spans, source identity, diagnostic codes |
| `jqf-resource` | request accounting, allocation, cancellation, work meters |
| `jqf-data` | format-neutral `Document` and `Value`, facts, provenance |
| `jqf-syntax` | lexer and parser, including `let`, `.@`, `.&` |
| `jqf-codec-core` | registration, demand, access, encode, record streams — no parser |
| `jqf-codec-*` | one crate per format; engine and selector see only core |
| `jqf-engine` | compile, plan, demand, execute |
| `jqf-builtins` | overload registry and evaluators |
| `jqf-runtime` | the only crate that owns threads |
| `jqf-sdk` | portable `execute` |
| `jqf-sdk-ffi` | C ABI (strict JSON; resident NDJSON feed) |
| `jqf-cli` | the `jqf` binary: argv, host I/O, RSS governor, `serve` |

The binary crate lives in `jqf-cli/`; the package name is `jqf`. Each crate's `README.md` is the surface; `CONTRACTS.md` is the invariant list.

## Parallelism

Two shapes run on ordered workers: explicit NDJSON, and default adjacent-values stdin. A worker publishes bytes, or the whole request re-runs serially. A parallel answer is therefore byte-identical to a serial one. `--workers auto` keeps small inputs on the serial path.

The detail is [Parallelism](parallelism.md).

## Embedding

Depend on `jqf-sdk` plus the codec crates you want. Registration is one line per codec; a build understands exactly the formats it registered. Route-named drives are crate-private — `jqf_sdk::execute` is the entry.

`jqf-sdk-ffi` is the C ABI. Python and Wasm bindings live under `bindings/`. Resource governance applies to embedded calls the same way it applies to the binary. The detail is [Embedding jqf](embedding.md).

Codecs are dependency edges, not feature flags. Builtin extension families (`ext-hash`, `ext-schema`, `ext-jsonpath`, `ext-net`, `ext-fuzzy`, `ext-redact`) are the one thing `jqf-sdk` gates by feature; all six are on by default.
