# Types

`type` has eleven members, five more than jq. The five extras exist because
TOML, CBOR, and MessagePack carry values JSON cannot spell, and jqf doesn't
flatten them.

| `type`                                                          | From                                                   |
| --------------------------------------------------------------- | ------------------------------------------------------ |
| `"null"` `"boolean"` `"number"` `"string"` `"array"` `"object"` | every format                                           |
| `"bytes"`                                                       | CBOR byte strings, MessagePack `bin`, jqft `0x"…"`     |
| `"localdate"`                                                   | TOML `2024-01-02`                                      |
| `"localtime"`                                                   | TOML `07:32:00`                                        |
| `"localdatetime"`                                               | TOML `2024-01-02T07:32:00`                             |
| `"offsetdatetime"`                                              | TOML offsets, CBOR tags 0/1, MessagePack timestamp ext |

```console
$ printf 'd = 2024-01-02\n' | jqf --input-format toml -c '.d | type'
"localdate"
```

> YAML is *not* on that list: YAML 1.2's core schema has no timestamp or binary
> type, so a bare date is a string and an explicit `!!timestamp` stays a tagged
> wrapper. See [YAML](yaml.md).

## Ordering and equality

One total order spans every kind:

```text
null < false < true < number < string < bytes
     < localdate < localtime < localdatetime < offsetdatetime
     < array < object
```

Within a kind: numbers compare by mathematical value, bytes lexicographically,
local temporals field-by-field and offset datetimes by **instant** (
`…T12:00:00Z` equals `…T14:00:00+02:00`). Different temporal categories are
never equal - a `localdate` is not the string of its spelling, which is what
makes a [semantic diff](diff-validate.md) semantic.

## What programs can do with them

Arithmetic on the extra kinds is a type error. The conversions are explicit:
`tostring` on a temporal is its RFC 3339 text while on bytes its unpadded
base64url, `tojson` quotes the same text, the time builtins (`fromrfc3339`,
`mktime`, `todate`, …) move between temporals, epoch numbers, and strings.

```console
$ printf 'd = 2024-01-02\n' | jqf --input-format toml -r '.d | tostring'
2024-01-02
```

`length` answers strings, bytes, arrays, objects, numbers (absolute value), and
null. The kind filters (`numbers`, `strings`, `scalars`, …) know all eleven
answers.

## Tags

A value can carry a tag wrapper — a YAML `!money`, a CBOR `cbor:tag:37`, a jqft
`@tag("money")`. `type` and every kind test look **through** the wrapper; `tag`
reads it (`null` when absent); equality and encoding still see it. Constructing
a new value drops it, the same law as [facts](facts.md).

## Porting jq programs

A ported `if type == "string"` falls through on a `localdate`.
`--types-as-strings` is the compatibility dial. It makes each temporal
materialize as its canonical text, so the program sees jq's six types.

```console
$ printf 'd = 2024-01-02\n' | jqf --input-format toml --types-as-strings -c '.d | type'
"string"
```

The dial covers the four temporal kinds only. Bytes stay bytes (they have no
agreed text form) and a byte string leaving for a text format is projected to
base64url **and reported** (see [Formats](formats.md)).
