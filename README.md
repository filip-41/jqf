# jqf

jq's language, for every format, that edits files in place without destroying them.

Same jq programs, faster, with memory controls, without touching the bytes that don't need to be touched.

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

With no format flags, jqf is a strict JSON-in, JSON-out jq. A named file's format follows its extension; stdin is JSON; `--input-format` / `--output-format` override either.

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

- **Editing.** `--edit`, `--in-place`, `--diff`. Assignments patch the original bytes. An identity run is byte-verbatim.
- **Facts.** Read and write comments, tags, and markup attributes with `.@` and `.&`.
- **`--explain`.** The plan the engine derived: demand class, pushdown, chosen route, cost snapshot.
- **`--max-rss`.** A physical-memory governor, on by default.
- **`--follow`.** A live tail. Records publish as they arrive; a truncated last record is held.
- **`serve`.** Compile once. Serve NDJSON sessions over a unix socket or loopback TCP.

## Performance

jqf is fast, and it uses much less RSS. On an internal panel of about 680 comparable cases (defaults against defaults, geometric mean of wall time), it is typically around 2× jq, 2× gojq, 1.5× jaq, and 3× yq. On large documents the RSS gap is often larger than the time gap — a 90 MB file that costs jq ~900 MB resident is ~110 MB here when the program only needs a count or a path. jaq is close on many cells; jqf does not win everywhere.

Those numbers come from an internal harness that is not in this tree. They are a snapshot, not a guarantee, and they vary with program and input. A few rows from that panel (median of 3, wall / peak RSS, rounded):

| Query | Data | jqf | jq | jaq |
| --- | --- | --- | --- | --- |
| `.users[0].id` | ~46 MB JSON, 50k users | 130 ms / 50 MB | 490 ms / 460 MB | 290 ms / 570 MB |
| `.users[25000:35000] \| length` | same | 110 ms / 60 MB | 470 ms / 460 MB | 280 ms / 570 MB |
| `.users \| length` | ~92 MB JSON, 100k users | 210 ms / 110 MB | 920 ms / 910 MB | 540 ms / 1130 MB |
| `reduce .users[] as $u (0; . + $u.score)` | same | 240 ms / 130 MB | 1340 ms / 920 MB | 620 ms / 1130 MB |

## Where jqf differs from jq

A jq program should answer the same here, except the places below. Everything else — more formats, `--edit`, `--follow` — is additive and does not change those programs. The catalogue is [Using jqf as jq](docs/from-jq.md).

**Exact arithmetic.** jq computes in IEEE doubles. jqf uses exact decimals. `0.1 + 0.2` is `0.3`, `[100.0, 200.0] | add` does not pick up binary-float error, and a 19-digit id keeps its low bits. This is the one divergence that can change a result silently. A pipeline that captured jq's output byte for byte will see different numbers; there is no switch for jq's rounding.

**Strict JSON.** jqf refuses `01`, `+1`, `.5`, `1.`, and invalid UTF-8 in strings, where jq repairs them. `--strictness lenient` opts the number classes back in. Invalid UTF-8 still refuses.

**Dates and bytes.** TOML, YAML, and CBOR keep dates and byte strings as themselves. `type` therefore has eleven answers, not jq's six (`"localdate"`, `"bytes"`, …). A ported `if type == "string"` program falls through. `--types-as-strings` makes the temporals look like strings; byte strings are not covered by that dial.

**Memory ceiling.** `--max-rss` is on by default at 80% of the machine. A runaway `[inputs]` is refused (exit 5) instead of an OOM kill. Raise it, or pass `--max-rss 0`. jq has no ceiling.

**Regex.** A Rust engine, not Oniguruma. Five catalogued corners differ: zero-width scan advances by codepoint, not byte; look-behind with no fixed width is accepted; the `l` flag is not Oniguruma-longest; a capture inside a zero-width look-ahead keeps the captured text; compile-error wording differs.

**CLI.** `--argfile` is not accepted (use `--slurpfile`). `--debug-dump-disasm` and `--debug-trace` are unknown options; `--explain` is the analogue. `--build-configuration` prints jqf's own provenance, not jq's. Format flags, `--edit`, and `--follow` are extra.

## Docs

- [Architecture](docs/architecture.md)
- [Usage](docs/usage.md) — formats, editing, facts, memory, exit codes
- [Using jqf as jq](docs/from-jq.md)
- [jqf(1)](docs/jqf.1)

## Install

```console
$ cargo install jqf
$ brew tap filip-41/jqf && brew install jqf
```

From git HEAD: `cargo install --git https://github.com/filip-41/jqf jqf`. From a clone: `cargo build --release -p jqf`. `make pgo` builds the profile-guided binary at `target/pgo/jqf` — use that for any number you will quote. `jqf --diagnostics` prints build provenance.

Stable Rust, edition 2024, MSRV 1.96. Shell completions (bash / zsh) live in `tools/completions/`. The man page is `docs/jqf.1`.

Dual-licensed MIT OR Apache-2.0.
