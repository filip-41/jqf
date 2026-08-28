# Explain and diagnostics

`--explain` prints the plan the
engine derived, `--diagnostics` prints machine-readable run facts, and
`--plan-out` / `--plan-file` pin the plan so it cannot drift between runs. None
of them changes stdout bytes.

## `--explain`

The plan, on stderr, around the ordinary output:

```console
$ echo '{"users":[{"name":"a"}]}' | jqf --explain '.users[].name'
jqf: explain: program: .users[].name
jqf: explain: class: identity=no modifies=no whole_document=yes input_family=no morsel_static=no
jqf: explain: demand: class=Fields(name) boundary=residual
jqf: explain: pushdown: .users
jqf: explain: ladder: morsel=yes range_locate=no
jqf: explain: topk: rows=0
jqf: explain: compile_time: 21us
"a"
jqf: explain: route: stream
jqf: explain: run_time: 96us
jqf: explain: cost: peak=174489 input=25 output=4 spill_disk=0
jqf: explain: lazy: deferred=1 materialized=0
```

The shape lines (`class`, `demand`, `pushdown`, `ladder`, `topk`) are decoded in
[Shape recognizers § Reading the plan](recognizers.md#reading-the-plan) and the
route names in [Demand and pushdown](demand.md#routes-and-the-ladder). The run
adds timings, the cost snapshot (ledger peak, input/output bytes, spill), and a
`lazy:` line reporting how many containers stayed deferred versus materialized —
the receipt for what the optimizer actually skipped.

## `--diagnostics`

`--diagnostics` emits one JSON object
per diagnostic on stderr, plus provenance and residency lines:

```console
$ echo '{"a":1}' | jqf --diagnostics .a
jqf: build=pgo profile=… allocator=mimalloc platform=aarch64-macos pcores=6 ecores=12 pcore_source=detected
1
jqf: diag {"code":1,"name":"ROUTE_SELECTED","revision":1,"class":"Informational","severity":"Info",…,"operand":"stream",…}
jqf: diag {"code":2,"name":"COST_SNAPSHOT","revision":1,…,"operand":"peak=174236 input=8 output=2 spill_disk=0",…}
jqf: diag_counts: COST_SNAPSHOT=1 ROUTE_SELECTED=1
jqf: precision_boundary_events=0 declined_deferrals=0
jqf: rss: current_rss=4816896 peak_rss=4866048 … rss_source=mimalloc retained_input=0 ceiling=109951162777
jqf: ledger: ambient=174236 current=1311 enforced=true
```

The `build=` line is the binary's provenance (`--build-configuration` prints the
same facts and exits). The `rss:` line is the physical governor's view and the
`ledger:` line the accounted one — the two ceilings of
[Memory and limits](memory.md).

Every diagnostic has a numbered row in the codes registry, and
`--explain-code ID` prints one without reading stdin:

```console
$ jqf --explain-code 2
code:       COST_SNAPSHOT
id:         2
revision:   1
class:      Informational
severity:   Info
meaning:    Ledger totals read at run end.
```

## Plan pinning

`--plan-out PATH` writes the compiled program's routing facts (the same facts
`--explain` prints) as a small versioned, byte-stable file, before any input is
read. `--plan-file PATH` reads one back and requires it to match. A mismatch is
a startup error, never a silent fallback:

```console
$ jqf --plan-out users.plan '.users[].name' users.json
"a"

$ jqf --plan-file users.plan '.users[].name' users.json
"a"

$ jqf --plan-file users.plan '.users[].age' users.json
jqf: plan file users.plan does not match the compiled program's routing facts (the plan cannot drift from the route)
```

The use case is CI: commit the plan next to the program, and a jqf upgrade or a
program edit that changes the route (loses a pushdown, drops a recognizer row)
fails the pipeline instead of quietly running slower.
