# Formats and codecs

Every format is a codec crate behind one format-neutral contract. The engine
only sees `jqf-codec-core` through registration, access, demand, encode, and
record stream interface. A format implements that interface for specific data
input type.

Formats are explicit. jqf does not sniff bytes. A named file's format follows
its extension, stdin is JSON, `--input-format` / `--output-format` always win.

```console
$ jqf --list-formats
$ jqf --help-format yaml
```

## The codec family

| Crate                   | Formats                                                       |
| ----------------------- | ------------------------------------------------------------- |
| `jqf-codec-json`        | JSON, JSONC, JSON5, NDJSON, JSON text sequences               |
| `jqf-codec-yaml`        | YAML                                                          |
| `jqf-codec-toml`        | TOML 1.0, TOML 1.1                                            |
| `jqf-codec-delimited`   | CSV, TSV                                                      |
| `jqf-codec-cbor`        | CBOR, CBOR sequences                                          |
| `jqf-codec-messagepack` | MessagePack                                                   |
| `jqf-codec-xml`         | XML                                                           |
| `jqf-codec-html`        | HTML (document and fragment)                                  |
| `jqf-codec-ini`         | Java properties, INI, dotenv                                  |
| `jqf-codec-jqft`        | jqft, jqfjson, jqfb — the [native formats](native-formats.md) |
| `jqf-codec-render`      | `render` — output only                                        |

One crate can export several registrations: the JSON crate registers five
formats, the ini crate three. Registration is one validated record per format —
descriptor (dialects, capabilities, extensions), decoder, encoder, and record
provider. Build understands exactly the formats it registered.

## Dialects

A format names *what* the bytes are while dialect names *which profile* of the
grammar reads or writes them. Dialect ids are versioned spellings
(`yaml.core@1`, `csv.rfc4180-header@1`) so a profile is a stable identity. The
first registered dialect is the default, `--input-dialect` / `--output-dialect`
can be used pick other dialects.

Input dialects and output dialects are different lists. A decode profile cannot
be asked of the encoder, and vice versa

```console
$ echo '{"a":1,}' | jqf --input-format jsonc -c .
{"a":1}

$ echo '{"a":1,}' | jqf --input-format jsonc --input-dialect jsonc.default@1 .
jqf: json.trailing-comma: trailing comma is not permitted: the input does not match the selected format or dialect
```

## Capabilities

A codec advertises a closed capability set, the CLI plans against it and refuses
what was not declared.

| Capability      | Meaning                                                                              |
| --------------- | ------------------------------------------------------------------------------------ |
| Edit            | span binding, an edit-render dialect, and a splice policy for [`--edit`](editing.md) |
| Record          | a physical record stream (NDJSON, json-seq, CSV) — framed byte ranges                |
| Adjacent values | a stream of complete adjacent texts (JSON texts, YAML `---` documents)               |

Edit is declared by JSON, JSONC, JSON5, TOML, YAML, CSV/TSV, CBOR, XML,
MessagePack, properties, INI, dotenv, and jqfb.

HTML, NDJSON, json-seq, and cbor-seq only decode and encode while refusing
`--edit`.

Record streams are framers over byte ranges. Each record's payload then goes
through the payload codec's ordinary access ladder. See
[Streaming](streaming.md).

## Strictness

`--strictness error|warn|strict|lenient` governs decode/encode. The default,
`error`, is jq. `strict` promotes advisories (eg. a raw NUL byte, a
record-stream advisory, a lossy projection) to failure (exit 5). `lenient` opts
jq's number grammar back in (`01`, `+1`, `.5`, `1.`) and plans serial.

Per-format grammar strictness (JSONC trailing commas, YAML schemas, TOML 1.0 vs
1.1) is a dialect specification.

## Values a format cannot spell

A value the target format cannot represent natively is written canonically and
**reported** - one stderr line per kind per run. A CBOR byte string becomes
base64url text in JSON, a TOML date becomes an RFC 3339 string in formats
without temporals, a tag wrapper encodes its payload. The warning names the
projection:

```console
$ printf '\x45\x68\x65\x6c\x6c\x6f' | jqf --input-format cbor -c .
"aGVsbG8"
jqf: warning: 1 byte string rendered as base64url text
```

An encode with **no** sound projection (eg. a nested container in a CSV cell, a
local date in CBOR) is a typed refusal.

## The small config formats

The `jqf-codec-ini` crate covers the flat config family. All values are strings,
nothing is type-inferred. All three carry comment facts and declare Edit.

| Format       | Dialect               | Shape                                                               |
| ------------ | --------------------- | ------------------------------------------------------------------- |
| `properties` | `properties.jdk@1`    | flat key → string; `#`/`!` comments, `\` continuations, JDK escapes |
| `ini`        | `ini.jqf-strict@1`    | root keys plus one `[section]` level; `;`/`#` comments              |
| `dotenv`     | `dotenv.jqf-strict@1` | flat; a leading `export ` is stripped; no `$VAR` expansion          |

```console
$ printf '[db]\nhost = h\n' | jqf --input-format ini -c .
{"db":{"host":"h"}}

$ printf 'export FOO=bar\nBAZ="a b"\n' | jqf --input-format dotenv -c .
{"FOO":"bar","BAZ":"a b"}
```

## Per-format pages

- [JSON, JSONC, JSON5](json.md)
- [YAML](yaml.md)
- [TOML](toml.md)
- [CSV and TSV](csv.md)
- [CBOR and MessagePack](cbor.md)
- [HTML and XML](html.md)
- [Native formats: jqft, jqfjson, jqfb](native-formats.md)
- [Render](render.md)
