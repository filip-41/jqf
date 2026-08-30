# jqf-codec-yaml Contracts

Invariants for this crate and for hosts. Type overview and examples live
in [README.md](README.md).

This crate does not evaluate programs, open files, resolve external
entities, or treat YAML 1.1 type tags as silent conversions. Family bind,
publish, and error laws live in [../CONTRACTS.md](../CONTRACTS.md).

## Membership

One format id, one registration. The catalog matches the decoder and the
encoder against the same dialect list; the factories dispatch on the
request's own dialect. Core is first in that list because an extension's
default input dialect is `descriptor.dialects()[0]`.

An output-profile dialect on decode, or an input-schema dialect on
encode, is a requirement mismatch.

## Stream

A YAML source is a stream of documents, not one document. Each decode
publishes one document and reports `consumed_offset`. Inter-document
trivia is space, tab, LF, and CR. The advertised CLI routes are adjacent
values, edit, and record.

Empty documents, `---` / `...` markers, and `%TAG` / `%YAML` directives
are stream structure. `%TAG` handles resolve in the parser; the graph
stores exact resolved tag text.

## Schemas

Resolution turns a plain scalar's text plus its tag into a category.

- Failsafe: map, seq, str. A plain scalar is always a string.
- JSON: those three plus null, bool, integer, and float. A plain scalar
  that matches none of the JSON regexes is a schema error.
- Core: the same seven tags. Unmatched plain scalars fall back to
  string; an empty plain scalar is null.

Quoted scalars are always strings. The bare `!` tag is non-specific and
forces the string category.

The number law is YAML 1.2.2 core: integers are `[-+]?[0-9]+` (leading
zeros stay integers), `0o[0-7]+`, and `0x[0-9a-fA-F]+`. Underscores,
binary `0b`, and uppercase radix prefixes are strings. Finite floats
follow the core float production and unify to exact decimals; overflow is
unrepresentable; `.inf` / `-.inf` are signed infinity; `.nan` variants
are the positive quiet NaN bits `0x7ff8_0000_0000_0000`.

Explicit non-core tags (`!money`, `!!binary`, `!!timestamp`, `!!set`,
`!!omap`) stay a tagged wrapper around the ordinary payload. This crate
does not base64-decode, timestamp-parse, or reshape them.

The YAML 1.1 merge-key tag is consumed at mapping close under the core
schema and never reaches scalar resolution as a key. On a non-key scalar
it is an ordinary non-core tag.

## Anchors

Anchor names are not unique identities. Within one document, an alias
resolves to the most recent preceding anchor with that name. A forward
alias is invalid. The binding history resets at each document boundary.

Alias occurrences share one document node with the anchor. A cyclic graph
cannot become a semantic value and fails with
`UnsupportedRepresentation`; the graph itself retains the cycle.

## Keys

Duplicate-key validation runs before object projection, under
`yaml.key-equivalence@1`. Kind and exact resolved tag must match.
Scalars then compare their tag-defined value: strings by Unicode scalar
sequence, integers by mathematical value, booleans and null by value,
floats numerically with `-0.0 == +0.0` and every YAML NaN equal to every
other YAML NaN. Integer `1` and float `1.0` remain distinct because their
tags differ. Anchors and presentation are ignored; aliases compare their
resolved targets. Sequences compare in order. Mappings compare as
unordered sets of recursively equivalent pairs.

A mapping whose keys are not all direct core strings does not project to
an object.

Resource exhaustion during comparison is an error, never "different".

## Comments

Leading comments attach as `yaml.comment@1` on the following node.
A same-line comment after a value is `yaml.comment_inline@1` on that
value. Comments below a closing block that belong to that block are
`yaml.comment_foot@1`. The document trailer is the root's foot.

## Encode

`yaml.stream-canonical@1` emits an empty byte stream for zero items and,
for each item, `---\n` + the canonical document + `\n...\n`.
`yaml.single-document@1` accepts exactly one item and emits the canonical
document plus a trailing LF, with no markers.

Canonical bytes: UTF-8 without BOM, LF only, two-space indent, a final
LF; flow collections with exactly one trailing comma per item; every
unwrapped core node carries exactly one explicit standard tag; every
scalar is double-quoted; integers are minimal decimal; finite floats
render through the binary64 shortest-round-trip form. Standard tags use
the `!!` spelling, local tags keep their exact `!suffix`, other URIs use
`!<...>` without decoding percent triplets.

One non-core tag layer per node, with a direct string, sequence, or
mapping payload. A nested layer or a tag directly around null, bool,
integer, or float is unrepresentable.

Located block encode replays authored `&name` / `*name`. Owned encode
re-emits shared heap values at every occurrence; a cyclic or deeply nested
owned tree hits the nesting ceiling.

`yaml.block@1` is the default human-readable profile: block collections,
plain scalars wherever core resolution round-trips them, `---` between
documents, no `...` terminator. A single-line string is plain when
reading it back yields the same string, and double-quoted otherwise. A
string containing a newline is a literal block scalar when that form
survives byte for byte, and double-quoted otherwise.

## Tags

The tag validator accepts every grammar-valid tag that is injective into
a YAML node identity. Two distinct stored tags that would emit the same
property are `CollidingTags`.

## Edit

The edit-render dialect is `yaml.jqf-1.0@1`. Behaviorally the block
profile; the separate identity is the edit lane's output namespace.

- Spliced blocks adopt the splice site's indentation. The indent step is
  the file's own smallest positive indent delta, defaulting to 2.
- Flow collections stay flow.
- A block-scalar edit replaces the whole `|` / `>` span.
- A write through an alias refuses. The node carries the format-neutral
  edit-refusal role; the host raises that prose and never patches the
  anchor's authored span.
- Multi-document sources ride the adjacent-value drive: one document per
  poll, each spliced against its own retained segment.
- An edited scalar keeps its authored style when that style still
  round-trips.

## Access

Slot 0 is whole-document complete and reopens in place for the next
adjacent document. Slot 1 is the exact-path located route and is never
reopened per value. A prune hint on the whole-document requirement omits
mapping members the program cannot read. The same hint on Exact omits
unread members of the located subtree after the graph is fully parsed.
Neither skips byte validation.

Input dialects retain the edit document's trailing byte. Output profiles
have the facade supply the item newline.

## Boundaries

The scan is terminal (no recovery dialect). Work is admitted
cooperatively during scan, parse, and materialize.
