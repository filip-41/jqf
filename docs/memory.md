# Memory and limits

`jqf` defines two ceilings: `--max-rss` watches the process's real,
physical resident set and is on by default and  `--max-memory-bytes` is the
accounted ledger, deterministic and off unless named. Crossing either is exit 5.

## `--max-rss`: the physical governor

**Default on at 80% of effective memory**, where effective memory is the smaller
of physical RAM and a detected cgroup/job limit, so a container's ceiling is the
container's, not the host's. Spellings: `--max-rss N` (with `k`/`m`/`g`
suffixes), `--max-rss N%`, and `--max-rss 0` to disable.

Crossing the ceiling is exit 5 with code `MACHINE_MEMORY`, after a
release-and-recheck grace pass (the allocator is asked to return memory before
the refusal stands). If detection fails, the governor degrades to measure-only
with a warning rather than guessing a ceiling.

```bash
jqf --max-rss 90% '[inputs]' big.ndjson   # raise it
jqf --max-rss 0   '[inputs]' big.ndjson   # turn it off
```

The governor watches the whole process: all [workers](parallelism.md) together,
and under [serve](serve.md) the whole daemon. A job that ran under jq and now
dies with `MACHINE_MEMORY` isn't broken — it's the ceiling doing its job. Raise
it or disable it.

## `--max-memory-bytes`: the accounted ledger

The ledger charges every tracked allocation to the request and refuses at a
named byte count, so the same input refuses at the same point on every machine.
It has no default ceiling; accounting always runs.

```console
$ jqf --max-memory-bytes 1000000 -nc '[range(1000000)]'
jqf: error: memory limit exceeded: the ceiling is 1000000 bytes, 84540 are already in use, and 524288 more could not be granted (raise the ceiling with --max-memory-bytes)
```

`--diagnostics` prints both views side by side: the `rss:` line (physical, with
its source and ceiling) and the `ledger:` line (accounted). See
[Explain and diagnostics](explain.md).

## The other dials

| Flag                       | Ceiling                                        |
| -------------------------- | ---------------------------------------------- |
| `--max-iterations N`       | frame transitions per run, a runaway-loop stop |
| `--max-spill-bytes N`      | accounted spill, 0 by default                  |
| `--max-spill-disk-bytes N` | on-disk spill, 0 by default                    |

```console
$ jqf --max-iterations 10 -nc '[range(100)]'
jqf: error: --max-iterations frame-transition ceiling exceeded
```

All of them are Tier-P flags, so a repository can pin its ceilings in
[`.jqf.toml`](configuration.md) instead of every invocation.

## Memory shape by route

Streaming routes hold one record plus the held tail ([streaming](streaming.md)),
so a live tail's residency doesn't grow with the file. A slurp (`-s`,
`[inputs]`) legitimately holds everything and is the usual way to meet the
ceiling. The lazy document keeps unread containers as spans rather than nodes
([demand](demand.md)), which is why a narrow program over a wide document stays
small.
