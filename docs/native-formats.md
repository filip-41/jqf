# Native formats: jqft, jqfjson, jqfb

jqf's value model is wider than JSON as it has bytes, four temporal kinds, exact
decimals, tags, and facts. No external format spells all of it (because it is,
by definition, a frankenstein monster composing all formats). The native family
exists to round-trip the **whole** model, value and facts, with nothing
projected and nothing warned.

> **Unstable.** The family is internal: no committed spec, no reader outside
> jqf. Use it for fixtures, tests, and pipeline handoff between jqf runs — do
> not persist data in it (or don't use it at all, you can totally ignore it)

| Format    | Kind                  | Spells bytes/temporals/tags | Edit |
| --------- | --------------------- | --------------------------- | :--: |
| `jqft`    | text, a JSON superset | in-grammar                  |  —   |
| `jqfjson` | strict JSON envelope  | refused, never projected    |  —   |
| `jqfb`    | binary chunk image    | in-structure                |  ✓   |

## jqft

A human-readable text form. A document must open with the `%jqft 1` directive;
documents are separated by `---`. On top of JSON literals it has:

- bytes: `0x"dead"` or `b64"…"`
- temporals in TOML spellings: `2024-01-02`, `07:32:00`, offsets
- binary64 marked with an `f` suffix (`1.5f`, `inf`, `-0f`) — an unmarked number
  is exact
- tag layers: `@tag("money") 5`, outermost first
- bare keys, trailing commas, `#` comments
- markup nodes `<name &attr="v" …>` carrying name/attr/content facts

```console
$ printf '%%jqft 1\n@tag("money") 5\n' | jqf --input-format jqft '.@tag'
"money"

$ printf '%%jqft 1\n0x"dead"\n' | jqf --input-format jqft type
"bytes"

$ echo '{"a":1}' | jqf --output-format jqft .
%jqft 1
{
  a: 1
}
```

Anchors, aliases, and non-string keys are refused with dedicated diagnostics to
not make jqft a second coming of YAML.

## jqfjson

The same document model in a strict RFC 8259 envelope, for consumers that must
stay JSON-parseable. A value the envelope cannot spell — bytes, a temporal, a
tag — is a typed encode **error**. Where ordinary JSON output would warn and
project, jqfjson refuses

## jqfb

The machine image: a `jqfb` magic header, typed chunks, and a footer directory
whose entries carry blake3 digests, verified before any chunk is consumed.
Unknown chunk types with the ignorable bit are skipped, unknown critical types
refuse. Subtrees are contiguous, so a located walk is a byte range.

jqfb is the one native format that declares Edit: a changed leaf rewrites from
that item to end-of-file while a structural change rewrites the container, its
ancestors' sizes, and the footer. `--with-source` embeds the original retained
source as a chunk alongside the tree.

```console
$ echo '{"a":[1,2]}' | jqf --output-format jqfb . > t.jqfb
$ jqf --input-format jqfb -c . t.jqfb
{"a":[1,2]}
```

## Dialects

| Format    | Input                | Output                |
| --------- | -------------------- | --------------------- |
| `jqft`    | `jqft.document@1`    | `jqft.canonical@1`    |
| `jqfjson` | `jqfjson.document@1` | `jqfjson.canonical@1` |
| `jqfb`    | `jqfb.document@1`    | `jqfb.canonical@1`    |

jqft and jqfjson are adjacent-value streams; jqfb is one document per source
(the codec owns its inter-item bytes — a facade newline would corrupt the
footer).
