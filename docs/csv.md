# CSV and TSV

The delimited codec frames physical records — boundaries, ordinals, quote-aware
cuts — and decodes each row's fields. CSV and TSV are separate registered
formats, so a tab file is never comma-parsed.

## Rows are arrays, `--header` makes objects

By default every record is an **array of field strings**. Row 1 is data. With
`--header`, row 1 is consumed as the schema and never published while every
later row decodes as an object keyed by it.

```console
$ printf 'a,b\n1,2\n' | jqf --input-format csv -c .
["a","b"]
["1","2"]

$ jqf --input-format csv --header -c . people.csv
{"name":"ada","age":"30"}
{"name":"bob","age":"25"}
```

`--header` is the one-word spelling of `--input-dialect csv.rfc4180-header@1`
plus its encode mirror. A duplicate header name is renamed `a`, `a_2`, `a_2_2`;
an empty name is kept.

The header is a whole-stream fact, so a drive that cannot read the stream whole
refuses it: `--follow` and a non-seekable stdin pipe both name the redirect that
works.

## Cells are strings

Field values are never type-inferred: `1`, `2.5`, and `true` decode as strings,
and `tonumber` stays explicit. Encode renders every scalar in its string form. A
nested container has no CSV spelling and refuses:

```console
$ printf '1,2.5\n' | jqf --input-format csv -c '.[0] | type'
"string"

$ echo '{"nested":{"x":1}}' | jqf --output-format csv .
jqf: codec failed: the value cannot be represented in the output format
```

## Dialects, delimiters, quoting

| Input                                       | Law                                                                                           |
| ------------------------------------------- | --------------------------------------------------------------------------------------------- |
| `csv.utf8@1` (default), `csv.utf8-header@1` | any valid UTF-8 scalar in a field                                                             |
| `csv.rfc4180@1`, `csv.rfc4180-header@1`     | the RFC's frozen TEXTDATA alphabet — TAB, C0, DEL, and non-ASCII refuse even when valid UTF-8 |
| `tsv.utf8@1`, `tsv.utf8-header@1`           | tab-delimited; **no quoting** — `"` is ordinary data                                          |

Quoting is RFC 4180: `"` opens only at field start, `""` is a literal quote, a
lone mid-field quote is malformed. On write, a field is quoted only when it
contains the delimiter, a quote, CR, or LF. CSV output rows end CRLF. TSV rows
end LF.

`--csv-delimiter BYTE` swaps the field delimiter to one ASCII byte the codec
accepts (`;`, `|`, `:`, space, alphanumerics; `\t` spells a tab). It is valid
only with CSV: the registered `tsv` format binds its own tab and rejects the
delimiter byte.

```console
$ printf 'a;b\n1;2\n' | jqf --input-format csv --csv-delimiter ';' -c .
["a","b"]
["1","2"]
```

Headered decode enforces the row width: a row with more or fewer fields than the
header is a ragged-row error. The array dialect has no width law.

## Encoding

Headerless output takes an array of scalars, or a flat object whose values
become the row. Headered output (`--header`, or the `*-header@1` output
dialects) takes objects: the **first** object's key order becomes the header
row, and every later object must match it exactly.

```console
$ printf '{"a":"1","b":"2"}\n{"a":"3","b":"4"}\n' | jqf --input-format ndjson --output-format csv --header .
a,b
1,2
3,4
```

## Editing

CSV and TSV declare Edit. A field's span is its raw authored bytes, quotes
included: a changed cell is spliced in place, keeping the authored quote style
where one existed. Headerless edits may grow or shrink a row, a headered edit
that changes the column set refuses (the header is shared state). TSV refuses a
spliced field containing TAB, CR, or LF — there is no quoting to hide them in.

```console
$ cat e.csv
name,age
ada,30

$ jqf --edit --in-place --input-format csv --header '.age = "31"' e.csv

$ cat e.csv
name,age
ada,31
```

A live tail (`--follow`) frames CSV records with RFC 4180 quote-state, a newline
inside quotes does not cut a record. See [Streaming](streaming.md).
