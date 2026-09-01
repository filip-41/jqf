# Builtins

jqf implements the whole jq 1.8.2 library, plus it's own extensions.
`jqf --list-builtins` prints every registered builtin as `name/arity`, one per
line (the same enumeration the `builtins` builtin answers, so the CLI surface
and the language surface share one source). `jqf --help <family>` documents each
family from the binary.

## The registry

Builtins live in an overload registry keyed by `(name, arity)`. Overloads are
validated at build, so that duplicate ids, duplicate name/arity pairs, and
missing docs are compile errors. A jqf-only builtin is always a new name to
avoid colisions with jq.

## The extension families

Six families are feature-gated in [embedded builds](embedding.md) and all on by
default (and always on in the CLI):

| Family         | Builtins                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `ext-hash`     | digests `md5` `sha1` `sha256` `sha512` `blake3` `xxhash` `crc32`; `hmac`, `hmac_sha256`, …; encoders `hex_encode`/`hex_decode`, `base64url_*`, `base32_*`, `percent_*`, `quoted_printable_*`; compression `gzip_*`, `zlib_*`, `deflate_*`; sets `union` `intersect` `except`; `uuid` `uuid_v4` `uuid_v7`; math `pi` `tau` `round/1..2` `signum` `fract` …; stats `avg` `median` `variance` `stddev` `quantile` `frequency` `pivot` `melt`; rand `rand` `randint` `choice` `sample` `shuffle`; `numfmt` |
| `ext-schema`   | `schema_validate` `schema_errors` `schema_infer` `schema_diff` — see [Diff and validation](diff-validate.md)                                                                                                                                                                                                                                                                                                                                                                                           |
| `ext-jsonpath` | `jsonpath/1` `jsonpath/2` — see [Selectors](selectors.md)                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `ext-net`      | `ip_valid` `ip_version` `ip_class` `ip_canonical` `ip_in_cidr`                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `ext-fuzzy`    | `edit_distance` `similarity` `fuzzy_match` — Levenshtein over NFC, case-folded                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `ext-redact`   | `redact/0..2` `redact_keyed` — whole-value, regex-substring, or HMAC-pseudonym redaction                                                                                                                                                                                                                                                                                                                                                                                                               |

Hash and encode builtins work on strings (`"abc" | sha256`), gzip output is
deterministic (mtime zero).

## Always-on jqf extras

Not feature-gated, always registered:

- **Selectors** — `css/1` over HTML, `xpath/1` over XML.
- **Diff** — `diff/2`, the same records `--diff` prints.
- **Pointers** — `json_pointer/1..2` (RFC 6901 get/set).
- **Facts** — `tag/0`, `json_facts/0`. See [Facts](facts.md).
- **Parsers** — `parse_url`, `parse_query_string`, `parse_user_agent`,
  `parse_syslog`, `parse_logfmt`, `parse_grok/1`.
- **Time** — `fromrfc3339` / `torfc3339` beside jq's whole time family.
- **Windows and running stats** — `windows/2`, `moving_avg/2`, `ewma/2`,
  `deltas/1`, `lag/1`, `running/2` — built for [`--follow`](streaming.md) live
  streams.
- **Kind filters** — `numbers`, `strings`, `booleans`, `scalars`, `finites`,
  `normals`, … as native evaluators of the `select(type == …)` law, aware of all
  [eleven types](types.md).
- **Capability riders** — `have_decnum` and `have_literal_numbers`, both `true`.

```console
$ echo '"https://x.dev/a?b=1"' | jqf -c 'parse_url | .host'
"x.dev"
```

## The engine surface

Four spellings in `--list-builtins` start with `~`: `~generator/3`, `~cursor/1`,
`~inputs/0`, `~rng/1`. They are not value builtins — the tilde marks the engine
surface, and they must bind with `as ~x`. They have
[their own page](generators.md).

## Prelude-backed names

Part of the enumeration is defined in the prelude rather than native code —
`all`, `any`, `first`, `last`, `values`, `nulls`, the window family — plus
`empty/0`, which is syntax. The enumeration does not distinguish, the law is
that every listed `name/arity` is callable.
