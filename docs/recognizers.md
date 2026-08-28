# Shape recognizers

After [fusion](engine-ir.md), analysis walks the arena against **closed tables**
of program shapes. A shape that matches a row lets the executor do less work
(visit fewer children, keep a bounded heap, answer from spans), and a shape that
isn't a row takes the ordinary floor, byte for byte. This page is the detail
under [Architecture § Shape recognizers](architecture.md#shape-recognizers).

Three rules hold for every recognizer:

1. **Closed tables, declining defaults.** Every recognizer enumerates what it
   accepts and everything else declines. A new shape joins by adding a row and a
   soundness argument, never by widening a default.
2. Recognizers change how the executor walks or what the document consumer
   answers. They never change published bytes, and a runtime decline falls back
   to the floor mid-flight without changing the answer.
3. Count and element demands are derived once at compile and consulted per
   record.

## Projection: `Structure < Fields(S) < Subtree`

Computed backward from publication at the program's element boundary (the sole
`.[]` the plan names). `Structure` means only shape is consumed (a count, a
`keys`), `Fields(a, b)` means only those top-level members, `Subtree` means
everything. Dynamic keys, indexes, or nested iteration join conservatively
upward, and past 64 named fields the class rounds up to `Subtree`. The class
feeds the [codec demand](demand.md) and the prune hints, and it is visible as
`demand: class=…` in `--explain`.

## Count

Rows: `PATH | length`, `PATH | keys | length`, `[C[] | suffix] | length`, and
the filtered twin `[C[] | select(p)] | length`. When the document can prove the
answer (a span skeleton knows its element count) the consumer publishes it
without walking elements. Optional probes, nested iteration, and paths the prune
tree cannot name decline.

## Element

Rows: the fan-out family (`.catalog[] | .name`, collected fan-outs, constructor
fan-outs with static keys), `reduce`-object-increment, and the counted prefixes
(`limit(k; …)`, `first`, `nth`). The consumer streams exactly the elements
needed, so `limit(2; .xs[])` visits two children, not the container.

## Correlated scan

The row is the join shape:

```jq
.orders[] as $o | .users[] | select(.id == $o.user_id) | .n
```

and its `map` spelling. The leftmost `==` under `select` (possibly under `and`)
is the key. One side must be a static path from the element and the other a
probe that doesn't read the element. The executor replaces the Θ(k·m) rescan
with an indexed walk of the container, keeping the same output sequence and
order. A raised build, a non-singleton probe, or an owned (unlocated) container
declines at runtime to the nested rescan. The anti-join spelling through
`all(…; …)` / `isempty` has its own row.

## Partial sort

A `sort` whose consumer only reads k elements doesn't need to sort the rest. The
rows and their consumers:

| Shape                                              | Consumer                     |
| -------------------------------------------------- | ---------------------------- |
| `sort \| .[0:k]`, `sort_by(f) \| .[0:k]`           | k smallest, bounded max-heap |
| `sort \| .[-k:]`, `sort_by(f) \| .[-k:]`           | k largest, bounded min-heap  |
| `sort_by(f) \| reverse \| .[0:k]`                  | k largest, descending        |
| `sort \| .[0]` / `first`, `sort \| .[-1]` / `last` | single extremum              |
| `sort \| reverse \| .[0]`                          | flips to last                |

The heap holds k entries, admission is `O(log k)`, the whole pass is
`O(n log k)` in `O(k)` memory. Ties preserve sort's stability: the smallest-side
heap admits an equal key only from an earlier index and the largest-side only
from a later one, so the kept elements are exactly the ones a full stable sort
would have placed in the window. NaN participates under sort's total order (one
point at the bottom). Non-constant bounds, a second consumer of the sorted
array, and `sort_by | .[0]` aren't rows and take the full sort.

`--explain` counts matched rows:

```console
$ echo '[5,1,4]' | jqf --explain -c 'sort | .[0:2]' 2>&1 | grep topk
jqf: explain: topk: rows=1
```

## Range locate

One row: a bare static path ending in a slice, like `.catalog[100:110]`. The
codec serves the slice by span (cut the container's byte range, decode only the
in-range elements). A non-array at the path falls through to the ordinary route.
Visible as `ladder: range_locate=yes`.

## Reading the plan

```console
$ echo '{"users":[{"name":"a"}]}' | jqf --explain '.users[].name'
jqf: explain: program: .users[].name
jqf: explain: class: identity=no modifies=no whole_document=yes input_family=no morsel_static=no
jqf: explain: demand: class=Fields(name) boundary=residual
jqf: explain: pushdown: .users
jqf: explain: ladder: morsel=yes range_locate=no
jqf: explain: topk: rows=0
"a"
jqf: explain: route: stream
jqf: explain: cost: peak=174489 input=25 output=4 spill_disk=0
```

| Line        | Meaning                                                                                           |
| ----------- | ------------------------------------------------------------------------------------------------- |
| `class:`    | program shape — identity, assignment, whole-document, `inputs`, static per-record path            |
| `demand:`   | the projection class and its boundary consumer (`none`, `residual`, `fold`, `binding`, `collect`) |
| `pushdown:` | the static prefix the codec serves, see [Demand](demand.md)                                       |
| `ladder:`   | parallel eligibility and range-locate, see [Parallelism](parallelism.md)                          |
| `topk:`     | partial-sort rows matched                                                                         |
| `route:`    | the route that served the request                                                                 |
| `cost:`     | ledger peak, input/output bytes, spill                                                            |

The rest of the observability surface (`--diagnostics`, `--explain-code`, plan
pinning) is [Explain and diagnostics](explain.md).
