# Usage

Formats, editing, facts, memory, and exit codes.
`jqf --help`, `jqf --help-format <fmt>`, and `jqf --help facts` are the live
surfaces.

## Build

Packaged on crates.io and Homebrew:

```console
$ cargo install jqf@0.1.0-alpha.2
$ brew tap filip-41/jqf && brew install jqf
```

From a clone:

```console
$ cargo build --release -p jqf
$ echo '{"name":"app","port":8080}' | target/release/jqf '.port'
8080
```

`make pgo` builds the profile-guided binary at `target/pgo/jqf` — use that for
any number you will quote. Stable Rust, edition 2024, MSRV 1.96.
`jqf --diagnostics` prints build provenance (including whether the binary was
profile-guided). `--build-configuration` prints the same facts and exits.

## Formats

Formats are explicit. jqf does not sniff bytes. A named file follows its
extension, stdin is JSON, `--input-format` / `--output-format` always win.

```console
$ jqf --list-formats
$ jqf --help-format yaml
$ jqf --input-format yaml --output-format json '.services | keys' stack.yaml
```

`--edit` is same-format, and only on codecs that declare it. HTML, NDJSON,
json-seq, and cbor-seq decode and encode; they refuse `--edit`.

Dialects (`--input-dialect` / `--output-dialect`) are the sharp choices most
runs never name: `ndjson.strict` vs `ndjson.recovering`, CBOR encode dialects,
YAML `block` vs `stream-canonical`.

**CSV / TSV.** `--csv-delimiter BYTE` (the registered `tsv` format binds its
own tab and rejects the dial). `--header` reads row 1 as keys; it is never
guessed. `--follow` cannot serve the headered dialect.

A value the target format cannot spell natively is written canonically and
**reported** (one stderr line per kind per run), never silently mangled.

Not shipped yet: Arrow, Parquet, Markdown, HCL, shell, and filesystem (a
directory tree as a jq document).

## Editing

`--edit` makes the document the output subject. Assignments patch the original
bytes; everything else stays the file you wrote.

```console
$ jqf --edit '.port = 9090' config.json

$ jqf --edit '.' config.json | diff - config.json && echo identical
identical
```

`--in-place` writes each positional file back to itself, independently and
atomically (new inode, then rename). File mode survives; hardlinks, ACLs, and
xattrs do not. `--no-atomic` writes the original inode.

```console
$ jqf --in-place --edit '.retries = 3' a.json b.json c.json
```

Without `--input-format`, the first file's extension selects the format for the
whole run. Mixed-format `--in-place` needs an explicit format or separate
invocations.

`--check` asks whether the edit would change the file and writes nothing (exit
1 if it would). `--diff` is a path-keyed semantic diff of two documents,
including across formats (`--old-format toml --new-format yaml`).

A program that produces zero or multiple outputs under `--edit` is an error.
`--in-place` is a usage error with `-n`, `-s`, `--diff`, `--follow`, or
`--output`.

**New bytes are canonical.** A leaf whose value changed is patched in place. A
value the program *constructed* is rendered fresh at the splice site. A
declined splice re-encodes the whole document; the result is re-decoded before
publish, so a fallback is correct, never corrupt.

**YAML aliases.** Patching a node shared by an alias would silently change
every alias site. The default is to refuse (exit 5). `--edit-expand-alias`
accepts that rewrite and warns once.

## Facts (`.@` and `.&`)

A node carries facts that are not its value: YAML tags, comments, markup
attributes. Read and write them on an ordinary run; `--edit` is what *splices*
the new bytes into the source. `jqf --help facts` is the same material from
the binary.

```console
$ echo '!money 5' | jqf --input-format yaml '.@tag'
"!money"

$ printf 'port = 8080 # main port\n' | jqf --input-format toml '.port.@comment_inline'
[
  "main port"
]

$ echo '<a href="https://x">y</a>' | jqf --input-format xml '.&href'
"https://x"
```

Comment positions: `.@comment` (leading), `.@comment_inline`, `.@comment_foot`.
`.@comment_head` is a second spelling of `.@comment`. A missing fact reads
`null`. Any operation that constructs a new value drops them:

```console
$ printf '# main port\nport = 8080\n' | jqf --input-format toml '(.port + 0) | .@comment'
null
```

XML and HTML input to JSON turns `--json-facts` **on by default**: markup
becomes an xq-style tree, because the bare value would drop every element name.

```console
$ echo '<a href="https://x">y</a>' | jqf --input-format xml -c .
{"a":{"@href":"https://x","#text":"y"}}

$ echo '<a href="https://x">y</a>' | jqf --input-format xml --no-json-facts -c .
["y"]
```

Root-level paths differ between the two dials. Probe with `.` first.

## Memory and residency

`--max-rss` watches the real resident set. **Default on: 80% of detected
effective memory** (physical RAM, or the cgroup/job limit). Crossing it is
exit 5, code `MACHINE_MEMORY`. `--max-rss 0` disables it; `--max-rss N|N%`
raises it.

`--max-memory-bytes` is a different thing: the accounted ledger, off unless
named. `--diagnostics` prints both (`rss:` vs the cost snapshot).

If detection fails, the governor degrades to measure-only with a warning.

`--follow` tails a growing file per record. `jqf serve --listen <socket|host:port>`
compiles once and serves NDJSON sessions; a per-value error does not kill the
session. A unix socket is filesystem-authenticated; a TCP listener is
trusted-network-only (no auth, no TLS).

## Exit codes

jq's classes.

| Code | Meaning |
| :-: | --- |
| 0 | success; under `-e`, a truthy last output |
| 1 | `-e` with false/null last value; `--diff` when they differ; `--edit --check` when the edit would change the file |
| 2 | usage or host/system failure |
| 3 | the program was rejected at compile time |
| 4 | `-e` with no output |
| 5 | runtime: parse failure, value failure, codec refusal, resource ceiling |
| N | `halt(N)` / `halt_error(N)`; bare `halt` is 0, bare `halt_error` is 5 |

Per-value errors that do not condemn the request keep exit 0.
`--explain-code ID` prints one diagnostic-code row and does not read stdin.

`--strictness error|warn|strict|lenient` governs decode/encode. Default `error`
is jq. `lenient` accepts jq's number grammar (`01`, `+1`, `.5`, `1.`) and
plans serial. `--mismatch-policy lenient|warn|strict` governs jq's
value-answering sites (missing key, out-of-range index). Default `lenient`
**is** jq.

```bash
jqf '.' config.json >/dev/null || echo "bad json"
jqf --edit --check '.version = "2"' config.toml || echo "would change"
jqf --diff old.toml new.toml --input-format toml >/dev/null || echo "drift"
```

## Common issues

- **A job that used to run under jq now dies with `MACHINE_MEMORY`.** The 80%
  RSS ceiling is on. Raise it (`--max-rss 90%`) or turn it off (`--max-rss 0`).
- **`01` / `+1` / `.5` / `1.` refuse.** Strict RFC 8259. `--strictness lenient`
  opts those number classes back in. Invalid UTF-8 still refuses.
- **`--in-place` on mixed extensions parses later files as the first file's
  format.** Pin `--input-format` or run them separately.
- **An inline TOML comment is `.@comment_inline`, not `.@comment`.** Leading
  block comments are `.@comment`.
- **Comments vanished after `.port + 0`.** Facts ride on the value. Constructing
  a new number drops them.
- **XML `.` is a tree, not the bare text.** `--json-facts` is on by default for
  markup→JSON. `--no-json-facts` asks for the bare value.
- **YAML `--edit` refused an alias.** Default is refuse. `--edit-expand-alias`
  if you want the shared-anchor rewrite.
- **`--debug-dump-disasm` / `--debug-trace` are unknown flags.** Use `--explain`
  / `--diagnostics`.
- **`type` grew extra answers** (`"bytes"`, `"localdate"`, …) on TOML / CBOR /
  YAML. `--types-as-strings` makes a ported jq program see jq's six types.
  See [Using jqf as jq](from-jq.md).
