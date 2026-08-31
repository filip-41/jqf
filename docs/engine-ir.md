# Engine IR

jq is a generator language, and the compiler honors that: no bytecode, no stack
VM. A program lowers to a **generator graph** — a dense arena of nodes indexed
by id, with one root — and the executor interprets that graph. This page is the
detail under [Architecture § Engine IR](architecture.md#engine-ir).

For the full compile pipeline (gate, preludes, lowering, transform, analyze,
finish), see [Engine compiler](engine-compiler.md).

## The composition core

Five node kinds carry the shape of the program:

| Node              | jq                      | Semantics                                                                 |
| ----------------- | ----------------------- | ------------------------------------------------------------------------- |
| `Stage`           | `.a.b`, `.[]`, literals | a static walk: a start (current input, a literal, a variable) plus steps  |
| `FlatMap`         | `a \| b`                | run the body once per upstream value                                      |
| `Choice`          | `a, b`                  | all of the left over the input, then all of the right over the same input |
| `CollectArray`    | `[…]`                   | collect the body's stream into one array                                  |
| `ConstructObject` | `{…}`                   | the Cartesian product of member streams, built pairwise                   |

Stage steps are the path vocabulary, each step
carrying its own `?` flag.

Control forms are IR nodes too, not a second representation: `if` lowers to
`Conditional`, `//` to `Alternative`, `try` to `Try`, `as` to `Bind`,
`reduce`/`foreach` to their own nodes, `label`/`break` to slot-addressed nodes.
On top of that, a non-recursive `def` inlines into its call sites, recursion becomes an explicit
callable, assignments lower to `Modify` and fact
assignments to `FactAssign`, a span delta for [the edit lane](editing.md).

`CountCollect` is the one fused special: `[body] | length` without ever
materializing the array.

## Fusion and path-normal form

Analysis fuses every `FlatMap(Stage, Stage-from-current)` into one stage
`.a | .b` becomes the single path `.a.b`. What blocks fusion is exactly what
changes semantics: a `Choice` or constructor on either side, and a literal-start
body (it ignores its upstream, so fusing would change cardinality). One
structural rewrite runs alongside: an object constructor whose members share a
static prefix hoists it — `{a: .p.x, b: .p.y}` → `.p | {a: .x, b: .y}` — except
a leftmost `==` under `select`, which is protected because the
[correlated-scan recognizer](recognizers.md#correlated-scan) needs it intact.

The arena at rest is in **path-normal form** - no fusable pipe of stages remains.
A bare-`Stage` root is a pure path, a `Choice` or constructor root is a
top-level comma or collection and `FlatMap` root survives only where one side
blocked.

## The pushdown split

From the normalized arena, one split is computed: descend from the root through
`FlatMap` upstream edges only (never into a `Choice`), find the entry stage, and
take its maximal prefix of static key/index steps **before the first `.[]`**.
That prefix becomes the codec's access requirement —
[the exact-path footprint](demand.md) — and everything from the first iteration
onward is the **residual** the executor drives. The residual is the same arena
with the pushed steps consumed, not a copy.

Identity is whole-document access, never an empty forward path — the distinction
between "give me the document" and "give me the value at ``" is load-bearing at
bind time.

```console
$ echo '{"users":[{"name":"a"}]}' | jqf --explain '.users[].name' 2>&1 | grep pushdown
jqf: explain: pushdown: .users
```

## Execution

Two machines dictated by shape:

- A bare-`Stage` residual takes the **single-slot fast path**: one task slot, a
  frame stack allocated lazily on the first `.[]` or `..`. A static path never
  allocates a stack at all.
- Anything else — `Choice`, surviving `FlatMap`, the control nodes — runs the
  **graph interpreter** on a unified frame stack.

Both obey the same discipline: **one value in flight per frame**. Fan-out is
never buffered and emission walks the live frames top-down and the first consumer
takes the value.

Worked intuition:

| Program         | Normal form               | Pushdown       | Machine              |
| --------------- | ------------------------- | -------------- | -------------------- |
| `.a \| .b`      | one stage `.a.b`          | `.a.b`         | fast path            |
| `.users[].name` | one stage                 | `.users`       | fast path from `.[]` |
| `.a, .b`        | `Choice`                  | whole document | graph                |
| `[.[] \| .x]`   | `CollectArray(FlatMap …)` | whole document | graph                |
| `.a = 1`        | `Modify`                  | whole document | graph                |

After normalization, analysis walks this arena against the closed
[recognizer tables](recognizers.md); the split and the recognizer verdicts
together become the plan that `--explain` prints.
