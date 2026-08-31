# Documentation

- [Usage](usage.md) — formats, editing, facts, memory, exit codes
- [Using jqf as jq](from-jq.md) — what carries over, and where it does not
- [Architecture](architecture.md) — document, engine IR, shape recognizers, crates
- [jqf(1)](jqf.1) — command-line reference

**Formats.** [Formats and codecs](formats.md) ·
[JSON, JSONC, JSON5](json.md) · [YAML](yaml.md) · [TOML](toml.md) ·
[CSV and TSV](csv.md) · [CBOR and MessagePack](cbor.md) ·
[HTML and XML](html.md) · [native formats](native-formats.md) ·
[render](render.md)

**Language.** [Numbers](numbers.md) · [Types](types.md) ·
[Builtins](builtins.md) · [engine constructors](generators.md) ·
[selectors](selectors.md) · [facts](facts.md)

**Workflows.** [Editing](editing.md) · [diff and validation](diff-validate.md) ·
[streaming and `--follow`](streaming.md) · [serve mode](serve.md) ·
[explain and diagnostics](explain.md) · [memory and limits](memory.md) ·
[configuration](configuration.md) · [embedding](embedding.md)

**Architecture in depth.** [Document model](document-model.md) ·
[engine IR](engine-ir.md) · [engine compiler](engine-compiler.md) ·
[shape recognizers](recognizers.md) · [demand and pushdown](demand.md) ·
[parallelism](parallelism.md)

`jqf --help`, `jqf --help-format <fmt>`, and `jqf --help facts` are the live
flag and format surfaces.


## Examples

### JSON — edit the file in place

```console
$ cat config.json
{
  "name": "app",
  "port": 8080,
  "tags": ["a", "b"]
}

$ jqf --in-place --edit '.port = 9090' config.json

$ cat config.json
{
  "name": "app",
  "port": 9090,
  "tags": ["a", "b"]
}
```

### YAML — add a key, leave the comments

```console
$ cat config.yaml
# Deploy owns this name. Changing it orphans the service registry.
name: checkout
# Public HTTPS port. Matches the Service spec.
port: 8080

$ jqf --edit '.retries = 3' config.yaml
# Deploy owns this name. Changing it orphans the service registry.
name: checkout
# Public HTTPS port. Matches the Service spec.
port: 8080
retries: 3
```

### TOML — change a value and write comments in one program

`.@` is jqf's accessor for comments.

```console
$ cat cfg.toml
name = "checkout"
port = 8080

$ jqf --edit '.port = 9090 | .port.@comment = ["owned by platform", "do not change without a ticket"] | .port.@comment_inline = ["matches the Service spec"]' cfg.toml
name = "checkout"
# owned by platform
# do not change without a ticket
port = 9090 # matches the Service spec
```

With no format flags, jqf is a strict JSON-in, JSON-out jq. A named file's
format follows its extension; stdin is JSON; `--input-format` /
`--output-format` override either.

## Formats

`jqf --list-formats` is the live table. `jqf --help-format yaml` is one page.

| Format | In | Out | Edit |
| --- | :-: | :-: | :-: |
| JSON | ✓ | ✓ | ✓ |
| JSONC / JSON5 | ✓ | ✓ | ✓ |
| NDJSON / JSON text sequences | ✓ | ✓ | — |
| YAML | ✓ | ✓ | ✓ |
| TOML | ✓ | ✓ | ✓ |
| CSV / TSV | ✓ | ✓ | ✓ |
| CBOR | ✓ | ✓ | ✓ |
| CBOR sequences | ✓ | ✓ | — |
| XML | ✓ | ✓ | ✓ |
| HTML | ✓ | ✓ | — |
| MessagePack | ✓ | ✓ | ✓ |
| INI / Java properties / dotenv | ✓ | ✓ | ✓ |
| `render` (tables and trees) | — | ✓ | — |

Not shipped yet: Arrow, Parquet, Markdown, HCL, shell, and filesystem (a directory tree as a jq document — walk a root, query names/sizes/types, load file contents through the codec registry).

## Beyond jq

jqf is a complete jq. It also edits files without rewriting them, follows live logs, and caps memory — things jq does not do.

- **Editing.** [`--edit`, `--in-place`, `--diff`](usage.md#editing). Assignments patch the original bytes. An identity run is byte-verbatim.
- **Facts.** Read and write comments, tags, and markup attributes with [`.@` and `.&`](usage.md).
- **`--explain`.** The plan the engine derived: demand class, pushdown, chosen route, cost snapshot.
- **`--max-rss`.** A physical-memory governor, on by default. See [memory and residency](usage.md#memory-and-residency).
- **`--follow`.** A live tail. Records publish as they arrive; a truncated last record is held.
- **`serve`.** Compile once. Serve NDJSON sessions over a unix socket or loopback TCP.

## Where jqf differs from jq

A jq program should answer the same here, except the places below. Everything else — more formats, `--edit`, `--follow` — is additive and does not change those programs. The catalogue is [Using jqf as jq](from-jq.md).

**Exact arithmetic.** jq computes in IEEE doubles. jqf uses exact decimals. `0.1 + 0.2` is `0.3`, `[100.0, 200.0] | add` does not pick up binary-float error, and a 19-digit id keeps its low bits. This is the one divergence that can change a result silently. A pipeline that captured jq's output byte for byte will see different numbers; there is no switch for jq's rounding.

**Strict JSON.** jqf refuses `01`, `+1`, `.5`, `1.`, and invalid UTF-8 in strings, where jq repairs them. `--strictness lenient` opts the number classes back in. Invalid UTF-8 still refuses.

**Dates and bytes.** TOML, YAML, and CBOR keep dates and byte strings as themselves. `type` therefore has eleven answers, not jq's six (`"localdate"`, `"bytes"`, …). A ported `if type == "string"` program falls through. `--types-as-strings` makes the temporals look like strings; byte strings are not covered by that dial.

**Memory ceiling.** `--max-rss` is on by default at 80% of the machine. A runaway `[inputs]` is refused (exit 5) instead of an OOM kill. Raise it, or pass `--max-rss 0`. jq has no ceiling.

**Regex.** A Rust engine, not Oniguruma. Five catalogued corners differ: zero-width scan advances by codepoint, not byte; look-behind with no fixed width is accepted; the `l` flag is not Oniguruma-longest; a capture inside a zero-width look-ahead keeps the captured text; compile-error wording differs.

**CLI.** `--argfile` is not accepted (use `--slurpfile`). `--debug-dump-disasm` and `--debug-trace` are unknown options; `--explain` is the analogue. `--build-configuration` prints jqf's own provenance, not jq's. Format flags, `--edit`, and `--follow` are extra.

## Install

```console
$ cargo install jqf
$ brew tap filip-41/jqf && brew install jqf
```

Docs and an in-browser playground run on GitHub Pages:
<https://filip-41.github.io/jqf/> — [playground](https://filip-41.github.io/jqf/assets/playground/).

From git HEAD: `cargo install --git https://github.com/filip-41/jqf jqf`.
From a clone: `cargo build --release -p jqf`. Stable Rust, edition 2024,
MSRV 1.96. Dual-licensed MIT OR Apache-2.0.
