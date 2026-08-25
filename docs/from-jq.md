# Using jqf as jq

jqf runs jq programs. Not a subset and not a dialect — the language of jq
1.8.2, with jq's flags, jq's exit-code classes, and jq's output bytes unless
this page says otherwise.

```console
$ echo '{"users":[{"name":"a","age":30},{"name":"b","age":25}]}' \
    | jqf '[.users[] | select(.age > 26) | .name]'
[
  "a"
]
```

If you already use jq, use jqf the same way. Reach for [Usage](usage.md) when
you want something jq does not do (formats, `--edit`, `--max-rss`).

## What carries over

**The language.** Paths, pipes, generators, `reduce`/`foreach`, `try`/`catch`,
`def`, `import`/`include`, string interpolation, the builtin library,
`$__loc__`, label/break, path expressions and assignment. `jqf --list-builtins`
prints every registered builtin as `name/arity`.

**The flags.** `-n`, `-r`, `-R`, `-s`, `-c`, `-e`, `-S`, `-j`, `-a`, `-C`,
`-M`, `-f`, `-L`, `--arg`, `--argjson`, `--args`, `--jsonargs`, `--slurpfile`,
`--rawfile`, `--tab`, `--indent`, `--raw-output0`, `--stream`, `--stream-errors`,
`--seq`, `--unbuffered`, and `-b` (accepted and ignored, as jq does on Unix).
Fiddly parts match: `-n` beats `-s`, `-R` beats `-s`, `-a` beats `-r` for a
root string, `-M` always beats `-C`, `JQ_COLORS` sets the eight-field palette.

**The exit codes.** 0, 1 under `-e`, 2 for usage, 3 for compile, 4 for `-e`
with no output, 5 for runtime, `halt(N)` / `halt_error(N)`'s own status.

**Shell habits.** Same argument order, same stdin, same `--`, same
`--unbuffered` flush.

## How compatibility is held

`make jq-suite` runs jq's own test suite as an oracle. `make compat` runs a
CLI corpus against system jq, byte for byte. Those receipts are the authority.
If a behaviour is not on this page and jqf disagrees with jq, that is a bug.

## Declined flags

`--debug-dump-disasm` and `--debug-trace` name jq's bytecode VM, which jqf
does not have. Both are unknown options (exit 2). `--explain` and
`--diagnostics` are the analogues.

`--build-configuration` is implemented: it prints this binary's provenance and
exits 0. `--argfile` is an unknown option; `--slurpfile` is the spelling.

## Intentional divergences

### Exact arithmetic

jq computes in IEEE doubles. jqf uses exact decimals. This is the one
divergence that can change a result silently.

| program | jq 1.8.2 | jqf |
| --- | --- | --- |
| `0.1 + 0.2` | `0.30000000000000004` | `0.3` |
| `0.1 + 0.2 == 0.3` | `false` | `true` |
| `9007199254740993 + 0` | `9007199254740992` | `9007199254740993` |

If a downstream consumer requires jq's exact bytes, pin the rendering yourself
(`tostring`, `@json`, rounding).

### Strict JSON

jqf's JSON reader is RFC 8259. jq accepts `01`, `+1`, `.5`, `1.`, huge
exponents (clamped), and substitutes U+FFFD for invalid UTF-8. jqf refuses
those (exit 5) rather than repairing them. `--strictness lenient` opts the
number classes back in; invalid UTF-8 still refuses. `fromjson` is already
jq-lenient.

### Extra `type` answers

`type` has eleven answers here, jq's has six. TOML / CBOR / YAML dates and
byte strings stay themselves (`"localdate"`, `"bytes"`, …). A ported
`if type == "string"` program falls through.

```console
$ printf 'd = 2024-01-02\n' | jqf --input-format toml -c '.d | type'
"localdate"
$ printf 'd = 2024-01-02\n' | jqf --input-format toml -c --types-as-strings '.d | type'
"string"
```

`--types-as-strings` makes the program see jq's six types; byte strings are
not covered by that dial.

### Default-on RSS ceiling

jq has none. jqf's `--max-rss` default is 80% of detected memory. A runaway
`[inputs]` is exit 5 (`MACHINE_MEMORY`) instead of an OOM kill. Raise or
disable it (`--max-rss 0`) for jobs that legitimately need the machine.

### Nesting

Structural nesting is capped at 10 000 levels (program, input, output). jq's
parser has the same cap; jqf also refuses a *constructed* value past it
(`reduce range(10001) as $i (null; [.])`). Linear iteration is not nesting.

### Regex

Rust engines, not Oniguruma. Five catalogued corners differ: zero-width scan
advances by codepoint not byte; look-behind with no fixed width is accepted;
the `l` flag does not mean Oniguruma-longest; a capture inside a zero-width
look-ahead keeps the captured text; compile-error wording differs.

### Other ruled corners

NaN-tied `sort` is a real total order here (jq's answer depends on the
platform). `0 * -1` is `0`, not `-0` (a decoded `-0` keeps its sign). A
handful of `?//` / `foreach` / `limit` / constructor-key exit-class rows are
pinned in the compat corpus.

### CLI extras

Format flags, `--edit`, `--in-place`, `--diff`, `--follow`, `serve`,
`--max-rss`, `--explain` are additive. They do not change jq programs that do
not use them.

## When to reach past jq

- [Usage: formats](usage.md#formats)
- [Usage: editing](usage.md#editing)
- [Usage: memory](usage.md#memory-and-residency)
