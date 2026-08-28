# Streaming input and `--follow`

jqf supports record streams, live tails usunf one core rule: a
record is complete only when its terminator has been seen, so a truncated tail is
**held**.

## Default stdin is adjacent values, not NDJSON

With no flags, stdin is a stream of adjacent RFC 8259 texts — complete JSON
values separated by whitespace. Newline-delimited input is *shaped* like NDJSON,
but NDJSON is never inferred: it has laws of its own (record ordinals,
blank-line and BOM rules, terminator classes) that only apply when you name it.

```console
$ printf '{"a":1} {"a":2}' | jqf -c .a
1
2
```

## NDJSON and JSON text sequences

| Format     | Framing                                            | Dialects                                 |
| ---------- | -------------------------------------------------- | ---------------------------------------- |
| `ndjson`   | newline (`--ndjson-terminator lf\|crlf` on output) | `ndjson.strict@1`, `ndjson.recovering@1` |
| `json-seq` | RS (0x1E, RFC 7464) — a boundary even mid-string   | `json-seq.strict@1`                      |
| `cbor-seq` | self-framing concatenation (RFC 8742)              | `cbor-seq.rfc8742-generic@1`             |

**Strict** stops at the first framing or payload fault. **Recovering** turns
faults into ordered issues and resumes at the next boundary. A complete final
record without a trailing newline is accepted under both as JSON Lines permits
it.

Exit codes: per-value errors that complete keep exit 0, but one
**error-severity** recovering issue forces exit 5 — a stream that had a
malformed record cannot exit clean:

```console
$ printf '{"v":1}\nnot-json\n{"v":2}\n' | jqf --input-format ndjson --input-dialect ndjson.recovering@1 -c .v; echo "exit=$?"
1
2
exit=5
```

The one exception is `--seq`, which selects RFC 7464 for both sides under a
flag-scoped recovering profile whose parse errors are never fatal (jq's
behavior): the exit stays with the last record's result. Every `--seq` output
item is prefixed with RS.

## `--follow`

jqf reads to end-of-file and then polls for
growth, each record publishes as its terminator arrives, and a truncated last
record is held until it completes. Rotation is handled `tail -F`-style: when the
path's inode changes, the old file is drained, the path reopened, and the tail
restarts — with one advisory on stderr.

```console
$ jqf --follow --unbuffered -c '.level' app.ndjson
```

Default framing under `--follow` is recovering NDJSON. Memory is bounded by the
one partial record: completed bytes are dropped as they publish.

`-n` combines with `--follow` only for input-family programs — that is the
live-window shape, with the [running-stats builtins](builtins.md):

```console
$ printf '{"ms":10}\n{"ms":20}\n{"ms":30}\n' | jqf --input-format ndjson -nc 'ewma(0.5; inputs | .ms)'
10
15
22.5
```

`--follow` refuses what has no incremental meaning: `-R`/`-s`, `--stream`,
`--edit`, `--diff`, `--in-place`, `--output`, and the headered CSV dialect (the
header is a whole-stream fact a refilling drive cannot carry — see
[CSV](csv.md)).

## `--stream`

jq's streaming parser: the filter runs once per `[path, leaf]` event instead of
once per value, so a program can react to a document too large to hold. JSON
input under `--stream` is bounded — an incremental parse holding only the path
stack. `--stream-errors` (implies `--stream`) turns parse errors into
`[message, path]` events and resumes.

```console
$ echo '{"a":[1,2]}' | jqf --stream -c .
[["a",0],1]
[["a",1],2]
[["a",1]]
[["a"]]
```

## Pipes and flushing

A non-seekable stdin already streams per record without `--follow`, the flag's
remaining job on a pipe is the EOF law (finalize the held tail). `--unbuffered`
flushes after every output item.

The same record-stream engine, fed over a socket instead of a file, is
[serve mode](serve.md).
