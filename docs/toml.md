# TOML

Two dialects: `toml-1.0` (the default, owns the `.toml` extension)
and `toml-1.1`. One document per source, comment facts, real temporal values,
and full [`--edit`](editing.md) support.

## Values

TOML dates and times decode as real temporals, not strings (see
[Types](types.md)):

```console
$ printf 'd = 2024-01-02T07:32:00\n' | jqf --input-format toml -c '.d | type'
"localdatetime"
```

Numbers are exact, tables become objects, arrays stay arrays. TOML has no null,
so encoding one refuses instead of inventing a spelling:

```console
$ echo '{"a":null}' | jqf -o toml .
jqf: codec failed: the value cannot be represented in the output format
```

## Dialects

`toml-1.1` accepts what TOML 1.1 adds to the grammar: Unicode bare keys, `\e`
and `\x` escapes, newlines and comments inside inline tables, and trailing
commas in inline tables. A leap second is forbidden in 1.1.

```console
$ printf 'x = {a = 1,}\n' | jqf --input-format toml -c .
jqf: toml.invalid-inline-table: trailing comma in inline table: the input does not match the selected format or dialect

$ printf 'x = {a = 1,}\n' | jqf --input-format toml --input-dialect toml-1.1 -c .
{"x":{"a":1}}
```

Output dialects mirror the pair: `toml.jqf-1.0@1` and `toml.jqf-1.1@1`.

```console
$ echo '{"server":{"port":8080,"tags":["a"]}}' | jqf -o toml .
[server]
port = 8080
tags = ["a"]
```

## Comments and editing

TOML carries all three comment positions as [facts](facts.md): `.@comment`
(leading), `.@comment_inline`, and `.@comment_foot`. Under `--edit` a value
change is a leaf patch that leaves them alone, and fact assignments splice
comment lines in place:

```console
$ printf 'port = 8080\n' | jqf --edit --input-format toml '.port = 9090 | .port.@comment_inline = ["matches the Service spec"]'
port = 9090 # matches the Service spec
```

An identity `--edit` run is byte-verbatim, so key order, whitespace, and
comments come back exactly as authored.
