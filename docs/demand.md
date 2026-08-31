# Demand and pushdown

The [recognizers](recognizers.md) name what the program needs and the codec
decides how much of the document to *build*. This page is the optimizer's
contract, the detail under
[Architecture § Codec demand](architecture.md#codec-demand).

> Demand is a hint that lets a route do less work, **never fewer guarantees**.
> Lazy whole-document is the floor; everything below it only changes how much
> gets materialized, not what gets validated.

## Three layers named apart

"Demand" means three different things, and the plan keeps them apart:

1. **The projection class** (`Structure < Fields(S) < Subtree`) says how much of
   each streamed element the residual consumes. Compile-time, per element
   boundary.
2. **The document demands** are the count/element/type questions a recognizer
   proved (`[C[]] | length` needs a count, not elements; bare `type` is a
   kind-only read). Derived once at compile, consulted per record.
3. **The codec demand clauses** are the format-neutral vocabulary a bind carries
   (semantic root, value shape, catalogued `.@` facts). A clause no route can
   serve is a **hard mismatch at bind**, not a silent repair. Markup `.&name`
   attributes bind through absence (JSON) or the whole-document fallback.

## Filter pushdown

The [pushdown split](engine-ir.md#the-pushdown-split) names the maximal
static prefix of the program before its first `.[]`. That prefix becomes the
codec's
`AccessRequirement` and the residual starts from the iteration. Two footprints
exist:

| Footprint | Serves                                |
| --------- | ------------------------------------- |
| `Exact`   | one static path (the pushdown prefix) |
| `Whole`   | the complete document                 |

Bind is first-match over the routes a codec advertises. An exact demand with no
Exact slot still opens Whole, since delivering more than asked is always sound.
Result authority is a hint and may fall back to the lazy whole document, so the
request's answer never depends on which codec honoured what.

Alongside the requirement rides a **prune tree**: the members the program
provably never reads, as an omission hint. A codec that honours it skips
*building* those nodes. It never skips validating their bytes.

## The validation floor

> **The validating scan visits every byte before any node is published.**
> Deferral changes whether a node is built, not whether a byte is validated. A
> corrupt byte in a field the program never reads fails the request exactly as
> the eager path would.

Which codecs can defer is a per-codec fact, owned by the codec's own module:

- **JSON** defers hardest. A lazy frontier leaves containers as validated spans,
  and counts, element streams, kind-only `type`, and `keys` come off the
  [span skeleton](document-model.md).
- **YAML** cannot: aliases need the whole anchor history and merge keys expand
  at container close, so the graph is built eagerly. Prune hints still shrink
  what the eager build keeps. On Exact, prune omits unread members of the
  *located* subtree after the full graph is parsed.
- **CBOR** and **MessagePack** honour prune on eager Whole (after every byte
  is validated) and on Exact re-decode of the located span. Prune and lazy
  never compose.
- **HTML** cannot: WHATWG recovery rewrites the tree, so located authority
  exists only after the whole recover. After recover, empty-path `length`
  and bare `type` may take a measure skeleton; `.[]` does not — HTML
  measure children are NAME-only stubs.

The engine never branches on which codec honoured the hint — it sees only
`jqf-codec-core`.

## Record streams

NDJSON, json-seq, CSV, and cbor-seq are **framers over byte ranges**, never
documents. The framer owns physical boundaries and ordinals, and each record's
payload then goes through the payload codec's ordinary access ladder (the same
Whole/Exact bind, reused per record, not a second decode stack). That's why
per-record pushdown works on a million-line NDJSON file: the split is computed
once and consulted per record.

## Routes and the ladder

The route is the drive that served the request and the ladder is the extras the
plan engaged. `--explain` prints both:

```console
$ echo '{"users":[{"name":"a"}]}' | jqf --explain '.users[].name' 2>&1 | grep -E 'demand|pushdown|ladder|route'
jqf: explain: demand: class=Fields(name) boundary=residual
jqf: explain: routes: count=no element=yes keys=no type=no inputs_cursor=no
jqf: explain: pushdown: .users
jqf: explain: ladder: morsel=yes range_locate=no
jqf: explain: route: stream
```

Route names you will see: `stream` (adjacent values / record streams), `record`,
`single-document`, `follow`, `edit`, `diff`, `sequence`, `stream-events`
(`--stream`), `range-locate`, `roundtrip`. The `lazy:` line after the run
reports how many containers stayed deferred versus materialized.

`--plan-out` / `--plan-file` serialize these routing facts and pin them, so the
route cannot drift silently between runs — see
[Explain and diagnostics](explain.md).
