# benchmark

jqf vs pinned [jq](https://github.com/jqlang/jq), [jaq](https://github.com/01mf02/jaq), [gojq](https://github.com/itchyny/gojq), [yq](https://github.com/mikefarah/yq), [dasel](https://github.com/TomWright/dasel), and [miller](https://github.com/johnkerl/miller) (`mlr`). Comparison, not a ranking. Tools land in `.deps/bin`.

jqf is always `target/pgo/jqf` (`make pgo`).

```console
$ make -C benchmark
$ make -C benchmark RUNS=30 WARMUP=2
```

`--runs` / `--warmup` override `cases.json` (default 3 timed, 1 warmup). `--quick` is size 100. Full panel is 100…200k × narrow/broad × json/ndjson/csv; yaml and toml stop at 100k. A kind only times tools that read that format.

Each finished case is stored under `.cache/cells/<stamp>/`; the stamp covers the workload, fixture generator, run settings, and competitor versions. A changed jqf build refreshes both jqf modes while retaining compatible competitor cells. `--force` remeasures everything. `--case GLOB` runs matching ids only; the report is rebuilt from compatible cached cells, not replaced. `exclude` in cases.json lists (id, tool) pairs with a why; `--with-excluded` times those. `.cache/results.md` and `.tsv` are rewritten after every case. `--out DIR` overrides the directory. The committed snapshot is [results.md](results.md).

A cell is timed only after stdout matches the oracle. Outputs above 1 MiB require identical SHA-256 digests; smaller JSON output is compared by typed values. Blank: `n/a`, `disagreed`, `timeout`, `error`.
