# JSON, JSONC, JSON5

All three formats are located in one crate (they share a number model and an
edit lane). JSON is the reference codec — the strictest reader and the fullest
capability set. JSONC and JSON5 are single-document supersets that keep
comments.

## Strict JSON

The default reader is RFC 8259 (jq repairs, jqf refuses strictly)

```console
$ echo '01' | jqf .
jqf: json.invalid-number: a JSON number cannot have leading zeros (the reference accepts them leniently; RFC 8259 forbids them): the input does not match the selected format or dialect

$ echo '01' | jqf --strictness lenient .
1
```

`--strictness lenient` opts jq's number grammar back in: leading zeros, explicit
`+`, `.5`, `1.`, and huge exponents clamped to the widest finite binary64.
Invalid UTF-8 in strings refuses under every setting. Numbers decode as exact
decimals either way — see [Numbers](numbers.md).

`fromjson` is a separate reader and is already jq-lenient: it accepts `+1`,
`01`, `nan`, and a leading BOM without any flag.

Stdin with no flags is a stream of **adjacent JSON texts** — complete values
separated by whitespace. Newline-delimited input is never inferred to be NDJSON.
See [Streaming](streaming.md).

```console
$ printf '{"a":1} {"a":2}' | jqf -c .a
1
2
```

## JSONC

RFC 8259 plus `//` and `/* */` comments. Two input dialects: `jsonc.trailing@1`
(the default) also accepts trailing commas, matching the tsconfig / VS Code
corpus; `jsonc.default@1` accepts comments only.

```console
$ echo '{"a":1,}' | jqf --input-format jsonc -c .
{"a":1}

$ printf '// deploy owns this\n{"a":1}\n' | jqf --input-format jsonc -c '.@comment'
["deploy owns this"]
```

A leading comment is a [fact](facts.md) on the node below it, readable as
`.@comment`. Inline and trailing comments survive `--edit` byte-for-byte but are
not projected as facts. JSONC declares Edit, splices are comment-aware.

## JSON5

One input dialect, `json5.document@1`. On top of JSON it accepts:

- `//` and `/* */` comments (as facts, same shape as JSONC), trailing commas
- single-quoted strings; `\x`, `\0`, and line-continuation escapes
- unquoted `IdentifierName` keys — ASCII only (`[A-Za-z_$][A-Za-z0-9_$]*`); a
  non-ASCII identifier is refused, quoted spelling always works
- hex integers, decoded as exact integers
- leading/trailing decimal points and explicit `+`
- `Infinity` and `NaN` (both signed)

```console
$ echo "{unquoted: 'ok', hex: 0xFF}" | jqf --input-format json5 -c .
{"unquoted":"ok","hex":255}
```

One named divergence from the JSON5 spec: `1e400` stays an exact decimal here
instead of overflowing to Infinity — the number model does not lose it.

## Output

JSON output is the default for every input format. The presentation dials are
jq's: `-c` compact, `--indent N` (0–7, `-1` for tabs), `--tab`, `-j`, `-r`,
`--raw-output0`, and `-a` for ASCII-escaped strings:

```console
$ echo '{"name":"café"}' | jqf -a -c .
{"name":"caf\u00e9"}
```

JSONC and JSON5 output dialects (`jsonc.jqf-1.0@1`, `json5.jqf@1`, …) exist for
the edit lane's re-render, so an edited `.jsonc` file stays JSONC.

## Capabilities

| Format  | In  | Out | Edit | Notes                                                |
| ------- | :-: | :-: | :--: | ---------------------------------------------------- |
| `json`  |  ✓  |  ✓  |  ✓   | record + adjacent-values routes (lazy scoped access) |
| `jsonc` |  ✓  |  ✓  |  ✓   | one document per source                              |
| `json5` |  ✓  |  ✓  |  ✓   | one document per source                              |

JSON is the codec that defers hardest: counts and element streams can be
answered from the validated span skeleton without building nodes the program
never reads. Every byte is still validated first. See
[Demand and pushdown](demand.md).
