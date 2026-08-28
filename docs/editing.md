# Editing documents

`--edit` makes the document the output subject. That means the program's assignments edit
one input document, and the edited document is published instead of the
expression outputs. Assignments patch the original bytes: comments, whitespace,
and key order survive, because they are never re-rendered.

```console
$ cat cfg.toml
name = "checkout"
port = 8080

$ jqf --edit '.port = 9090 | .port.@comment = ["owned by platform"] | .port.@comment_inline = ["matches the Service spec"]' cfg.toml
name = "checkout"
# owned by platform
port = 9090 # matches the Service spec
```

## The splice model

Three laws govern every edit:

1. **Exactly one output per document.** A program that produces zero or multiple
   outputs under `--edit` is an error.
2. **Any doubt takes the floor.** A changed scalar leaf with a retained span is
   patched byte-exactly. A value the program *constructed* is rendered fresh at
   the splice site. A change the splice policy cannot place (an insert the
   format cannot locate, a structural reshape) re-encodes the whole document
   instead of guessing.
3. **Re-decode before publish.** The patched bytes are decoded again and
   compared against the program's output. A fallback is therefore correct, never
   corrupt.

The identity program is the degenerate case (nothing changed, so the output is
byte-identical to the input)

```console
$ jqf --edit '.' config.json | diff - config.json && echo identical
identical
```

Fact assignments (`.port.@comment = […]`) are span deltas, not value mutations
That means that the node's comment lines are rewritten in place, but only comment-carrying formats
(TOML, YAML, JSONC, JSON5, properties, INI, dotenv) can splice those bytes. A
strict-JSON `--edit` fact write is a usage error. See [Facts](facts.md).

Edit is same-format, on every codec that declares it: JSON, JSONC, JSON5, TOML,
YAML, CSV/TSV, CBOR, XML, MessagePack, properties, INI, dotenv, and jqfb. HTML,
NDJSON, json-seq, and cbor-seq refuse it. YAML refuses an edit through an alias
unless `--edit-expand-alias` accepts the shared-anchor rewrite — see
[YAML](yaml.md).

## `--check`

The `gofmt -l` verdict for the edit lane: exit 0 when the would-be output is
byte-identical to the input, exit 1 when the edit would change the file.

```console
$ jqf --edit --check '.version = "2"' config.toml || echo "would change"
would change
```

## `--in-place`

Reads every positional file as the input *and* writes the output back to it.
Each file is edited independently, one run per file, and that file's output written
to itself. The output format defaults to the input's, so a `.yaml` file stays
YAML; `--output-format` opts into a conversion. All files must use one input
format: `--input-format` or `--seq` pins it, otherwise their detected extensions
must agree. With `--edit`, a file's original trailing bytes are preserved.

```console
$ jqf --in-place --edit '.retries = 3' a.json b.json c.json
```

`--in-place` is a usage error with `-n`, `-s`, `--diff`, `--follow`, and
`--output`. The reasoning is that a run over null, a slurp across files, or a diff pair has no single
coherent file to write back to.

## Atomicity

File destinations are written atomically by default: a temp file in the same
directory -> data fsync -> rename over the original -> directory fsync. The original
is untouched until the rename, and the rename is all-or-nothing.

The atomic replace is a **new inode**. The honest consequences:

| Survives                                      | Does not survive                                      |
| --------------------------------------------- | ----------------------------------------------------- |
| file mode (copied to the temp file)           | hardlinks — the sibling keeps the old inode's content |
| owner, best-effort when the process may chown | ACLs, xattrs, labels                                  |

`--no-atomic` is the same-inode escape: it writes the original inode directly,
so hardlinks and xattrs survive a successful run at a potential cost of a partial file
if the run fails mid-write. It requires `--output`, `--in-place`, or
`--split-exp`.

## `--output` and `--split-exp`

`--output PATH` writes the output to one file instead of stdout, atomically
under the same law.

`--split-exp EXPR` is the third destination model: one file per published item,
its path the expression's single string output evaluated over that item, with
`$index` bound to the 0-based item counter.

```console
$ printf '{"name":"a"}\n{"name":"b"}\n' | jqf --input-format ndjson --split-exp '"out/" + .name + ".json"' .
$ ls out
a.json  b.json
```

A missing parent directory is an error naming the path. `--split-exp` is
exclusive with `--output`, `--in-place`, `--edit`, and `--diff`, and with any
binding named `index`.
