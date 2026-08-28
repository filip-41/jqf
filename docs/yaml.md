# YAML

YAML 1.2.2. The codec keeps comments, anchors, tags, styles (as
[facts](facts.md) and document topology). This codec also supports edits files
with byte-range precision.

## Schemas

Three input dialects:

| Dialect           | Tags resolved                         | Unmatched plain scalar                |
| ----------------- | ------------------------------------- | ------------------------------------- |
| `yaml.core@1`     | null, bool, int, float, str, map, seq | string; an empty plain scalar is null |
| `yaml.json@1`     | the same seven                        | schema error — refuse                 |
| `yaml.failsafe@1` | map, seq, str                         | everything is a string                |

YAML 1.1 spellings are not silently converted: under core, `yes` is the string
`"yes"`, not `true`.

```console
$ echo 'yes' | jqf --input-format yaml -c .
"yes"

$ echo '42' | jqf --input-format yaml --input-dialect yaml.failsafe@1 -c .
"42"

$ echo 'yes' | jqf --input-format yaml --input-dialect yaml.json@1 -c .
jqf: yaml.schema: plain scalar does not match any JSON-schema tag: the input does not match the selected format or dialect
```

A bare `2020-01-01` under core is a **string** — YAML 1.2 has no timestamp in
its core schema. An explicit `!!timestamp` stays a tagged wrapper around the
text.

## Anchors and aliases

An alias refers to the most recent preceding anchor of that name, a forward
alias is invalid, anchor history resets per document. In the document topology,
the anchor and every alias site share **one node** — reading through an alias
answers the anchored value:

```console
$ printf 'a: &x {k: 1}\nb: *x\n' | jqf --input-format yaml -c .b
{"k":1}
```

That sharing is why editing through an alias refuses by default: patching the
shared node would rewrite the anchor's authored span and silently change every
other alias site.

```console
$ printf 'a: &x 1\nb: *x\n' | jqf --edit --input-format yaml '.b = 2'
jqf: pipeline.edit-refusal: editing through an alias is refused: the value is referenced by an alias, and rewriting its authored span would silently change every other alias site: the value cannot be represented in the output format
```

`--edit-expand-alias` accepts exactly the rewrite the refusal describes, and
warns once on stderr when it actually engages.

Merge keys (`<<`, the YAML 1.1 merge tag) are honoured under the core schema:
host keys override, earlier merge-sequence items win, chains resolve.

```console
$ printf 'base: &b {x: 1, y: 2}\nuse:\n  <<: *b\n  x: 9\n' | jqf --input-format yaml -c .use
{"y":2,"x":9}
```

## Tags, styles, comments

All three are facts — metadata on the node, not the value.

```console
$ echo '!money 5' | jqf --input-format yaml '.@tag'
"!money"

$ printf 'hello: world\n' | jqf --input-format yaml '.hello.@style'
"plain"
```

`.@style` answers `plain`, `single`, `double`, `literal`, or `folded`, and is
writable under `--edit` — as are `.@tag`, `.@anchor`, and `.@alias`:

```console
$ printf 'foo: bar\n' | jqf --edit --input-format yaml '.foo.@style = "double"'
foo: "bar"
```

Comments are `.@comment` (leading), `.@comment_inline`, and `.@comment_foot`,
readable everywhere and spliced in place under `--edit`. A non-editing run that
adds a key keeps every comment already in the file:

```console
$ printf '# deploy owns this\nname: checkout\n' | jqf --edit --input-format yaml '.retries = 3'
# deploy owns this
name: checkout
retries: 3
```

## Documents and output

A YAML source is a stream of documents (`---` / `...` markers, directives), not
one document. Each document is one input value.

Output profiles:

| Dialect                   | Shape                                                                                               |
| ------------------------- | --------------------------------------------------------------------------------------------------- |
| `yaml.block@1`            | default — block collections, plain scalars where they round-trip, `---` between documents           |
| `yaml.stream-canonical@1` | every node explicitly tagged, double-quoted scalars, flow collections, `---` and `...` per document |
| `yaml.single-document@1`  | exactly one item, no markers                                                                        |
| `yaml.jqf-1.0@1`          | the edit lane's re-render namespace                                                                 |

## No deferral

YAML is a codec that cannot defer materialization: aliases need the full anchor
history, merge keys expand at mapping close, and duplicate-key detection
precedes projection. The whole graph is built and every byte validated before
any node is published — the engine still sees an ordinary document, it just
costs a whole decode. See [Demand and pushdown](demand.md).
