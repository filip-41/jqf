# jqf-codec-jqft Contracts

Invariants for this crate and for hosts. Type overview and examples live
in [README.md](README.md).

This crate does not evaluate programs, open files, or promise a
cross-version jqfb archive. Family bind, publish, and error laws live in
[../CONTRACTS.md](../CONTRACTS.md).

## Membership

Three format ids, one crate. The grammar is sealed per registration, not
an option. A union grammar is a silent mis-parse: a bare key is legal in
jqft and illegal in jqfjson; `2.5f` is a binary64 in jqft and invalid
JSON; a jqfb footer newline would corrupt the image.

`jqft.document@1` is the crate-defined core document model. `jqfjson.document@1`
is strict JSON (RFC 8259 member-name and number laws). `jqfb.document@1`
is the encoder/decoder pair; the layout is pinned only as far as it
exists.

## jqft (`jqft.document@1`)

- A source begins with `%jqft 1`. Missing the header is a terminal
  failure.
- Documents in one source are separated by `---` after insignificant
  whitespace. The adjacent-value drive skips the JSON whitespace set
  between documents.
- Core scalars: `null`, `true` / `false`, exact numbers, binary64 with an
  `f` suffix (`inf`, `-0f`), JSON strings, `0x"…"` / `b64"…"` bytes,
  TOML-shaped temporal literals.
- `@tag("name")` layers are retained as tagged values, outermost first.
  Chains are repetition.
- `#` line comments parse. They do not become the value. Trailing commas
  and bare keys are legal. A key that is not a bare identifier is a
  quoted string or a `(…)` bracket form.
- A markup node `<name &attr="v" children…>` decodes as an array of its
  ordered children, with name / attributes / content attached as facts.
- Anchors, aliases, namespaced markup names, and non-string object keys
  are refused with a dedicated diagnostic, never dropped.
- The parser is iterative. Nesting depth costs heap, not call stack.

## jqfjson (`jqfjson.document@1`)

- Strict JSON. Member names are strings. Numbers follow the JSON number
  grammar.
- One envelope per source on the whole-document route. Adjacent envelopes
  are the adjacent-value lane, not a record stream.
- Trailing content after one document is a terminal failure unless the
  adjacent-value contract is on.
- Bytes, temporals, and tags have no spelling on this profile.

## jqfb (`jqfb.document@1`)

- One binary document per source. Not an adjacent-value format and not a
  record stream.
- Header: `jqfb` magic, `u16` LE version, `u32` LE flags. Then chunks.
  Then a footer directory ending in an 8-byte footer length.
- A reader seeks to the last 8 bytes, reads the footer, and validates
  every entry against the file extent before touching any chunk.
- Each directory entry is type, absolute offset, byte length, and a
  32-byte blake3 digest. Every chunk digest is verified before any byte
  of that chunk is consumed.
- The high bit of a chunk type marks it ignorable. An unknown ignorable
  chunk is skipped. An unknown critical chunk refuses the file.
- v1 critical chunks: `NODE` (preorder node table), `STRG` (string/bytes
  pool), `NUMB` (number pool). `FACT` is critical when present (a duplicate
  refuses); a missing FACT chunk is an empty fact table. v1 ignorable
  chunks: `PROV` (provenance), `SOUR` (retained source).
- A node's subtree is contiguous. `subtree_size` is the number of node
  entries the subtree occupies, self-inclusive. The reader checks that
  invariant. A malformed image is a typed error, never a panic and never
  an out-of-bounds read.

The jqfb layout has no external reader and no committed byte-level spec.
Do not persist archives in this format expecting cross-version stability.

## Value model

Null, bool, exact number, binary64, string, bytes, the four temporal
categories, array, and object round-trip through jqft and jqfb.
jqfjson round-trips the JSON subset and refuses the rest.

Tags are first-class grammar on jqft and jqfb and are retained as tagged
values. The registration records the no-tags validator: encode emits
`TagId` text from the node table, so nothing routes through the
validator channel.

Comments are not the value. Markup children are reached by `.[]`, never
a children fact. Duplicate keys follow the object-builder law: first
insertion fixes position, last occurrence supplies the value.

The scan is terminal (no recovery dialect) and corrupt-late: a bad byte
anywhere fails the whole document.

## Encode

jqft canonical form: `%jqft 1` on the first stream item, `---` between
items, two-space indent, one comma style, bare keys where legal,
JSON-escaped strings, exact numbers canonically, binary64 with the `f`
suffix, bytes as `0x"…"`, temporal literals TOML-shaped, `@tag("name") `
prefixes for retained tag layers.

jqfjson canonical form is compact JSON. A value jqfjson cannot spell
(bytes, temporals, tags) is a typed error, never a silently thinner
file.

jqfb encode writes the header, the critical chunks, optional ignorable
chunks, and the footer directory. `with_source` requests the retained-
source chunk. A level the run cannot supply is a typed error.

## Edit

jqft and jqfjson advertise adjacent-value, not edit. jqfb advertises
edit.

A jqfb leaf splice replaces the tail from the changed item through EOF:
the node's table entry when its pool home or kind changed, the value's
pool entry, and the footer words. Bytes between those regions copy
verbatim.

A jqfb structural splice rewrites the one container whose count moved,
re-derives ancestor `subtree_size` values, and rewrites the footer.
Orphaned pool entries are left in place. A span the source contradicts
declines to the whole-document floor, never wrong bytes.

## Registration

- `jqft`: extension `jqft`. Dialects `jqft.document@1` /
  `jqft.canonical@1`. Adjacent-value. Facade inter-item bytes.
- `jqfjson`: extension `jqfjson`. Dialects `jqfjson.document@1` /
  `jqfjson.canonical@1`. Adjacent-value. Facade inter-item bytes.
- `jqfb`: extension `jqfb`. Dialects `jqfb.document@1` /
  `jqfb.canonical@1`. Edit. Codec inter-item bytes (a facade newline
  would corrupt the footer).

The text formats advertise one access slot, whole-document complete. A
richer demand is served by core's whole-route fallbacks. jqfb advertises
whole-document complete and an exact/located subtree walk.

## Boundaries

The only jqf dependencies are `jqf-codec-core`, `jqf-data`,
`jqf-resource`, and `jqf-source`. This crate does not evaluate programs,
open files, or parse any other format family.
