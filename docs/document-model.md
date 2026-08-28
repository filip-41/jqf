# The document model

`jqf-data` holds two data models: **`Value`**, the owned
semantic value a jq program computes with, and **`Document`**, the immutable
format-neutral document a codec builds. This page is the detail under
[Architecture § Document](architecture.md#document).

## `Value`

The full variant list:

```text
Null | Bool | Number | String | Bytes
| LocalDate | LocalTime | LocalDateTime | OffsetDateTime
| Tagged { tag, payload } | Array | Object
```

Numbers are the tri-state of [Numbers](numbers.md): exact integer, exact
decimal, binary64. 

`Value` deliberately has **no `Eq`, `Ord`, or `Hash`**. Floats, temporals, tags,
and objects disagree about what "equal" means under any single substitute, so
the caller that compares owns the law it compares under — jq ordering, instant
equality, sort's total order — each an explicit choice in `jqf-builtins`.

Values share structure: the heap is behind reference-counted sharing, and clone
is a refcount, so fan-out over a large array does not copy it.

## `Document`

Decode produces one immutable `Document`. Everything
inside is a dense id local to that document — `NodeId`, `OccurrenceId`, `FactId`
— and the only handle that crosses a crate boundary is a `NodeHandle` (document
id + node id).


- **Occurrence topology** is what the source really was. It may keep duplicate
  keys, and its edges may be shared or cyclic (YAML anchor and its aliases
  are one node with many occurrences ([YAML](yaml.md))).
- **Semantic projection** is what a jq program sees. Projecting an object keeps
  the **first key's position** and the **last value** (the jq law for duplicate
  keys) and reading through an alias lands on the shared node.

## Facts

Facts are ordered, portable metadata attached to nodes ([Facts](facts.md)). Three laws:

1. A fact cannot change intrinsic meaning — it is provenance, not data.
2. Fact payloads are their own small vocabulary (text, lists, maps, bytes…),
   **not** `Value` — a fact cannot smuggle a document into a document.
3. Constructing a new `Value` drops facts, only located nodes carry them.

Where a node has an intrinsic tag (YAML `!!tag`, CBOR tag) *and* attached facts,
the intrinsic tag is authoritative for tag reads.

## Lazy documents and the span skeleton

A codec that can defer hands the engine containers that are still **source
spans**. Touching one
materializes it through the retained source. Two consumers exploit the skeleton
directly:

- counting a container's elements (`.items | length`) walks span boundaries,
  building nothing
- streaming elements (`.items[]`) cuts each element's span and materializes one
  at a time

The floor never moves: the validating scan visited every byte before any node
was published, so a corrupt byte in a field the program never reads fails the
request exactly as the eager path would. Deferral changes whether a node is
*built*, never whether a byte was *validated*. See
[Demand and pushdown](demand.md).

## Provenance and spans

Every node remembers where it came from: spans into the sealed source segment
(`jqf-source` owns span and source identity). Those authored spans are what
makes [`--edit`](editing.md) a byte patch — the edit lane replaces a leaf's span
in the retained source, and untouched bytes stay the file you wrote. Markup
attributes keep fact-level spans, which is how `.&href = "…"` splices one
attribute without re-serializing the element.
