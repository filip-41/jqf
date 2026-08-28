# Configuration

A `.jqf.toml` file can default some flags, so a repository can pin its
indentation or memory ceiling without wrapping the binary. Precedence, highest
first: argv, `--config PATH`, the nearest `.jqf.toml` (current directory or an
ancestor), the global file, built-in defaults. The global file lives at
`~/Library/Application Support/jqf/.jqf.toml` on macOS and
`$XDG_CONFIG_HOME/jqf/.jqf.toml` (`~/.config/jqf/` otherwise).

Only the file's `[defaults]` section is read.

## Two tiers

Only presentation and resource flags (Tier P) can come from a file. Semantic
flags (Tier S) — anything that changes what a program means, reads, or writes,
like `--input-format`, `--output-dialect`, or `--edit` — are argv-only, so a
config file can never change what a script's invocation does. A semantic key in
`[defaults]` warns and is ignored:

```console
$ printf '[defaults]\ninput-format = "yaml"\n' > bad.toml
$ echo '{"a":1}' | jqf --config bad.toml .
jqf: warning: config file bad.toml: input-format is a semantic (argv-only) flag and is ignored in [defaults]
{
  "a": 1
}
```

## `--show-config`

`--show-config` prints the effective configuration and the origin of every value
(argv, a config file, or a built-in default), then exits without reading stdin.
Its output doubles as the list of Tier-P keys:

```console
$ jqf --show-config
# effective .jqf.toml configuration
[defaults]
# color = auto  # built-in default
indent = 2  # built-in default
# max-memory-bytes = unlimited  # built-in default
max-rss = "80%"  # built-in default
max-spill-bytes = 0  # built-in default
max-spill-disk-bytes = 0  # built-in default
parallel = true  # built-in default
workers = "auto"  # built-in default
diagnostics = false  # built-in default
explain = false  # built-in default
unbuffered = false  # built-in default
mismatch-policy = "lenient"  # built-in default
strictness = "error"  # built-in default
```

With a file in play, the origin comment changes:

```console
$ printf '[defaults]\nindent = 4\n' > .jqf.toml
$ jqf --show-config | grep indent
indent = 4  # config file
```

## Hermetic runs

`--no-config` reads no configuration file at all, and a non-empty
`JQF_NO_CONFIG` in the environment does the same. Use either when a run must not
depend on what directory it started in (CI, cron, anything reproducible).
