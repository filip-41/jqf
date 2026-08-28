# Diff and validation

jqf has three separate surfaces: `--diff` compares two documents semantically,
`--edit --check` asks whether an edit would change a file (see
[Editing](editing.md)), and `--schema` gates every input value through a JSON
Schema. All three are exit codes first.

## Semantic diff

`--diff OLD NEW` reads each side as exactly one document and prints their
path-keyed semantic diff. Exit 0 when equal, 1 when they differ, so the flag is
a drift gate on its own.

```console
$ jqf --diff old.json new.json
[
  {
    "path": ["port"],
    "kind": "changed",
    "old": 8080,
    "new": 9090
  },
  {
    "path": ["tags"],
    "kind": "added",
    "new": ["a"]
  }
]
```

The output is one array of records sorted by path. Each record is
`{"path", "kind"}` with `kind` one of `added`, `removed`, `changed`, plus `old`
/ `new` as applicable. Objects recurse over the union of keys, arrays compare
positionally, and a kind mismatch is one `changed` record carrying both subtrees.
Equal paths emit nothing. The same comparison is the `diff/2` builtin:

```console
$ jqf -n -c 'diff({"a":1}; {"a":2,"b":3})'
[{"path":["a"],"kind":"changed","old":1,"new":2},{"path":["b"],"kind":"added","new":3}]
```

**Cross-format sides compare values, not text.** Each side defaults to
`--input-format`; `--old-format` / `--new-format` name a side that differs. A
TOML datetime on one side and the same text as a YAML string on the other is
`changed`, because temporal ≠ string.

```console
$ jqf --old-format toml --new-format yaml --diff pin.toml pin.yaml -c
[{"path":["at"],"kind":"changed","old":"2020-01-01T00:00:00Z","new":"2020-01-01T00:00:00Z"}]
```

A file containing multiple documents is a usage error naming the count, stdin is
never read.

```bash
jqf --diff old.toml new.toml --input-format toml >/dev/null || echo "drift"
```

## `--schema`: validate inputs

`--schema FILE` validates every input value against the schema document in FILE
before the program sees it. The schema file is read as exactly one strict JSON
value — the schema is always JSON, whatever `--input-format` the data uses — and
is bound as `$__schema`. The program (default `.`) runs only on values that
validate. A failing value prints its ordered error records to stderr and the
request exits **3** (the rejection class, distinct from a usage error's 2 and a
decode error's 5).

```console
$ cat schema.json
{"type":"object","required":["name"],"properties":{"name":{"type":"string"}}}

$ echo '{"name":"svc"}' | jqf --schema schema.json -c .
{"name":"svc"}

$ echo '{"port":8080}' | jqf --schema schema.json -c . ; echo "exit=$?"
[{"instance_path":"$","schema_path":"#/required","keyword":"required","message":"missing required property \"name\""}]
exit=3
```

`--schema` cannot combine with `--stream`, `--edit`, `--diff`, or `--in-place` —
none of them has a per-input-value stream to gate.

## The schema dialect

The profile is JSON Schema **2020-12** — core, validation, and applicator
vocabularies: `type`, `enum`, `const`, the numeric/string/array/object
constraints, `properties` / `items` / `prefixItems` / `required`, `allOf` /
`anyOf` / `oneOf` / `not` / `if` / `then` / `else`, local `$ref` into `$defs`.
Plus the jqf value-model vocabulary (`urn:jqf:value-model:1`) so `bytes`, the
temporal kinds, `decimal`, and tag wrappers are schemable.

The edges are explicit: `format` is annotation-only and never
asserts, a remote `$ref` is an error, an unknown `$vocabulary` URI is an error,
the known-unsupported keywords (`unevaluated*`, dynamic refs) raise a catchable
error rather than validating vacuously.

## The schema builtins

The same engine is a builtin family (`ext-schema`, on by default):

| Builtin                                    | Answer                                             |
| ------------------------------------------ | -------------------------------------------------- |
| `schema_validate(V; S)`                    | `true` / `false`                                   |
| `schema_errors(V; S)`                      | the ordered error array — empty when valid         |
| `schema_infer(V)`, `schema_infer(V; OPTS)` | a 2020-12 schema inferred from a value             |
| `schema_diff(A; B)`                        | drift records between two schemas, `diff/2`-shaped |

```console
$ jqf -n 'schema_validate({"id":1}; {"type":"object","required":["id"]})'
true

$ jqf -n -c 'schema_errors(3; {"minimum":4})'
[{"instance_path":"$","schema_path":"#/minimum","keyword":"minimum","message":"must be >= 4"}]

$ jqf -n -c 'schema_infer({"b":"x","a":{"n":1}})'
{"type":"object","required":["a","b"],"properties":{"a":{"type":"object","required":["n"],"properties":{"n":{"type":"integer"}}},"b":{"type":"string"}}}
```

Error records name both sides of the failure: `instance_path` into the value,
`schema_path` into the schema, the `keyword`, and a message. `schema_infer/2`
takes dials (`arrays: "items"|"length"`, `required: "observed"|"none"`,
`numbers: "loose"|"bounds"`, `strings: "type"|"enum"|"numeric"`) for how
aggressively to generalize.
