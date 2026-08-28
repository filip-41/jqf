# CBOR and MessagePack

Two binary codecs. Both round-trip the full value model and declare Edit (jqf
splices binary files the same way it splices text)

## CBOR

Input is `cbor.rfc8949-generic@1`: RFC 8949's generic data model, with UTF-8
text required, map-key uniqueness enforced, and the simple-value registry
honoured. One document per source, `cbor-seq` is the stream spelling.

| CBOR                          | jqf value                                          |
| ----------------------------- | -------------------------------------------------- |
| unsigned / negative integer   | exact integer                                      |
| byte string (major 2)         | `bytes`                                            |
| text string                   | string                                             |
| array / map                   | array / object, map keys must be unique UTF-8 text |
| simple true / false / null    | boolean / null                                     |
| floats (half, single, double) | binary64; non-finites round-trip                   |

Indefinite-length items are accepted. A map with a non-text key has no object
spelling and is refused as unrepresentable.

### Tags

Recognized tags project to real values while everything else is retained as a
tag wrapper the program can read with `tag` and `.@tag`.

| Tag           | Projection                                   |
| ------------- | -------------------------------------------- |
| 0             | RFC 3339 text → `offsetdatetime`             |
| 1             | epoch seconds → `offsetdatetime`             |
| 2 / 3         | bignum bytes → exact integer                 |
| 4             | decimal fraction → exact decimal             |
| 5             | bigfloat → number                            |
| anything else | `Tagged` wrapper, tag spelled `cbor:tag:<n>` |

```console
$ printf '\xc2\x42\x01\x00' | jqf --input-format cbor -c .
256

$ printf '\xc0\x74\x32\x30\x32\x30\x2d\x30\x31\x2d\x30\x31\x54\x30\x30\x3a\x30\x30\x3a\x30\x30\x5a' | jqf --input-format cbor -c type
"offsetdatetime"
```

On encode, an integer outside `[-2^64, 2^64-1]` becomes tag 2/3, an exact
decimal becomes tag 4, an `offsetdatetime` becomes tag 0. A local date or time
has no offset and refuses — CBOR cannot spell it.

### Output dialects

| Dialect                     | Law                                                                                  |
| --------------------------- | ------------------------------------------------------------------------------------ |
| `cbor.source@1`             | echo the sealed source bytes when the item is unchanged; otherwise preferred         |
| `cbor.preferred@1`          | shortest argument widths, definite lengths, shortest exact float; map order retained |
| `cbor.core-deterministic@1` | preferred, plus map keys sorted by encoded bytes and canonical NaN                   |
| `cbor.length-first@1`       | keys sorted by length, then bytes                                                    |

```console
$ echo '{"a":1,"b":[true,null]}' | jqf -c --output-format cbor . | jqf --input-format cbor -c .
{"a":1,"b":[true,null]}
```

### Editing binary

`--edit` splices a changed item's header-through-payload bytes in place. A
container that grows or shrinks rewrites only its count-bearing head. Indefinite
containers splice before the BREAK. The patched document is re-decoded before
publish, like every edit.

## CBOR sequences

`cbor-seq` (`cbor-seq.rfc8742-generic@1`) is RFC 8742. It decodes and encodes;
it refuses `--edit`.

```console
$ printf '\x01\x02' | jqf --input-format cbor-seq -c .
1
2
```

## Bytes in text formats

A byte string leaving CBOR or MessagePack for a text format is projected
canonically and reported, never silently mangled:

```console
$ printf '\x45\x68\x65\x6c\x6c\x6f' | jqf --input-format cbor -c .
"aGVsbG8"
jqf: warning: 1 byte string rendered as base64url text
```

## MessagePack

Input dialects: `messagepack.utf8@1` (default) and
`messagepack.key-equivalence@1`, which additionally rejects maps whose keys
collide under native equivalence. Only `str`-keyed maps become objects. The
`bin` family decodes as `bytes`. ext type −1 (timestamp) becomes
`offsetdatetime` and any other ext `n` is retained as a `msgpack:ext:<n>` tag
around its bytes.

Output is `messagepack.deterministic@1` (shortest markers, map order preserved).
`messagepack.deterministic-float64@1` additionally renders exact decimals as
their nearest binary64, lossy by declaration, for consumers that only read
floats. MessagePack declares Edit. A structural grow/shrink declines the splice
and re-encodes the document.

```console
$ echo '{"a":1}' | jqf -c --output-format messagepack . | jqf --input-format messagepack -c .
{"a":1}
```
