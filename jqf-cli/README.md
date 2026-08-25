# jqf

The command-line facade: argument parsing, request planning, route
drivers, and the process exit surface.

The binary crate lives in `jqf-cli/` so `cargo install jqf` publishes
the `jqf` name. It owns host I/O, the allocator, and the RSS governor.
Format grammar, program semantics, and drive selection stay in the
library crates beneath it.

What it has:

- `args` — formats, dialects, bindings, and the help text
- `config` — `.jqf.toml` defaults for presentation and resource flags
- `plan` / `routes` — the closed route inventory and one driver per lane
- `input` / `output` — stdin, files, stdout, and atomic file replace
- `errors` — exit classes 2 / 3 / 5
- `colour` — JSON-family ANSI rendering that cannot change decided bytes
- `rss` — the process RSS governor
- `discovery` — `--list-builtins`, `--list-formats`, `--help-format`, `--explain-code`
- `serve` — one compiled program, one record stream per connection

## Invocation

`PROGRAM` is at most one filter argument. Omitting it runs identity.
Formats are never guessed from content: a named file follows its
extension, stdin is JSON, and `--input-format` / `--output-format`
always win.

```text
jqf [OPTIONS] [PROGRAM]
jqf serve --listen <unix-socket|host:port> [PROGRAM]
```

```text
$ echo '{"n":1}' | jqf '.n + 1'
2
```

`-n` runs once over `null`. `-s` runs once over the array of every
decoded input. `-r` prints a root string without quotes. `-R` reads
each line as a raw string.

## Routes

`plan::resolve` builds an ordered chain from the request. Each
candidate serves, or declines and publishes nothing. `Execute` is
always last and never declines.

```text
Follow → Stream → Record → StreamEvents → Edit → Diff
       → NullFirst → Slurped → ValueLane → Execute
```

`--follow` and a non-seekable record stdin take the live stream.
`--edit` / `--diff` / `--in-place` own the document as the output
subject. `--workers` may take the sharded value lane; a plan that
cannot shard declines to serial.

## Exit

| Class   | Code | When                                      |
|---------|------|-------------------------------------------|
| Usage   | 2    | bad flags, missing files, host I/O        |
| Compile | 3    | the program was rejected before it ran    |
| Runtime | 5    | a value failed, or the input would not parse |

`halt` / `halt_error` exit with the status they name. `-e` overlays
the last truthiness on a successful run.

## Config

Tier P flags (colour, indent, RSS, workers) may default from
`.jqf.toml`. Tier S flags (the program, `-n` / `-s` / `-r`, formats)
are argv-only.

Precedence, highest first: argv, `--config PATH`, the nearest
`.jqf.toml` walking up from cwd, then the platform global file.
`--no-config` and a non-empty `JQF_NO_CONFIG` read nothing.

## Contracts

See [`CONTRACTS.md`](CONTRACTS.md) for argument, route, exit, and
publication invariants.
