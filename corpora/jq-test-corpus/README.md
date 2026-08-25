# jq-test-corpus

A self-contained corpus of **jq programs (expressions) and their test files**,
gathered from the upstream jq-family implementations, for testing any
jq-like tool (jqf, jq, gojq, jaq, …). Built by `extract.py` from the
checkouts listed below; `manifest.json` pins the exact commits.

## Why this corpus is carried

Retained deliberately as migration cargo, not consumed by any standing lane
as of 2026-08-24: the byte-oracle duty (`tools/jqf-cli-jq-compat.sh`,
`tools/jqf-jq-test-suite.py`) is covered by the vendored `tools/jq-test-suite/`
files, and this corpus exists so future differential campaigns (per-source
cross-tool runs against jaq/gojq/jqjq expectations) can sample `cases/*.test`
without a re-fetch. If a campaign adopts it, wire the lane and delete this
note; if it is still unconsumed at the migration cut, drop the directory then.

## Layout

```
sources/       pristine upstream test files, per implementation (the source of truth)
expressions/   one deduplicated jq program per line, per source + merged
cases/         runnable jq.test-format cases (program / input / expected outputs)
manifest.json  provenance: repo, commit, license, counts
fetch.sh       re-fetch upstream checkouts (pinned commits) and rebuild
extract.py     the parser: sources/ -> expressions/ + cases/
```

## What is here (built 2026-08-02)

| source | repo | commit | license | expressions | cases |
|---|---|---|---|---|---|
| jqlang-jq | https://github.com/jqlang/jq | `603db3f` | MIT | 764 | 1357 |
| jq-1.8.2 (pinned) | vendored in jqf `tools/jq-test-suite/` | file | MIT | 507 | 550 |
| jaq | https://github.com/01mf02/jaq | `78ea2d3` | MIT | 1057 | 1151 |
| gojq | https://github.com/itchyny/gojq | `2e210b5` | MIT | 10 | – |
| jqjq | https://github.com/itchyny/jqjq | `9f2553d` | MIT | 175 | 179 |
| exercism jq track | https://github.com/exercism/jq | `0a9a9ef` | MIT | 155 | – |
| **total unique programs** | | | | **2124** | 3237 |

`expressions/libraries.txt` lists 38 additional large standalone programs
copied verbatim into `sources/` (the standard libraries `src/builtin.jq`
(jq), `builtin.jq` (gojq), `jaq-{core,std,json}/src/defs.jq`, and the
`examples/*.jq` / `examples/benches/*.jq` programs).

### Per-source contents

- **jqlang-jq** — `tests/jq.test` (~550 cases), `tests/man.test` (~200),
  `optional.test`, `uri.test`, `base64.test`, `onig.test`, `tests/shtest`,
  `tests/modules/`, `src/builtin.jq`, and the manual examples from
  `docs/content/manual/v1.7` + `v1.8` (`manual.yml`, ~490 `- program:`
  entries) and the tutorial's `jq '…'` commands. `shtest` is copied as a
  test file but not extracted (shell/CLI-behavior tests, filters not
  machine-readable).
- **jq-1.8.2** — the official 1.8.2 `jq.test`, pinned, as a second oracle
  version.
- **jaq** — the Rust test files `jaq-{core,std,json,fmts}/tests/*.rs`
  (`give`/`gives`/`yields!`/`fail` cases), `jaq/tests/golden.rs` (CLI golden
  tests), `jaq/tests/{a,b}.jq` + `mods/` (include/import fixtures),
  `docs/*.dj` documentation examples (`prog --> outputs` lines, input
  `null`), `examples/*.jq` + `examples/benches/*.jq`, and the three
  `defs.jq` standard libraries.
- **gojq** — `cli/testdata/*.jq` (+ `.json`/`.yaml` inputs; no expected
  outputs shipped upstream, hence no cases) and `builtin.jq`.
- **jqjq** — `jqjq.test`, jq's own test format.
- **exercism-jq** — the whole `exercises/` tree: concept/practice `*.jq`
  scaffolds and `.meta/exemplar.jq` solutions (one expression per file) and
  `test-*.bats` files (which run `jq -f file.jq` or inline `run jq '…'`
  programs — the 102 inline programs are also extracted).

## Formats

`cases/*.test` use the jq.test format understood by `jq --run-tests`:
blank-line-separated groups of `<program>`, `<input JSON>`, then zero or
more expected-output lines. Every case is preceded by a `# source:` comment
naming the upstream file and test. Extension markers:

- `!error` — the program must fail; the message is not pinned
  (jaq `fail(...)` cases; jq's runner has no error-expected convention).
- `!skip (…)` — the case cannot be normalized to standalone JSON (Rust
  closures/variables in the test, raw-string values, multi-line or
  multi-value inputs, `--arg`/`--rawfile` fixtures, CLI-flag-dependent
  golden outputs); the original source text is kept in the marker.
  Counts: 32 jaq `!skip`, 9 golden `!skip`, 41 total; 9 `!error`.

`expressions/*.txt` have one program per line; internal whitespace is
collapsed (multi-line programs become single lines; escapes such as `\n`
inside strings are preserved as written). `all-unique.txt` merges all six
lists, first-seen order.

## Provenance and known drift — read this before comparing tools

- The corpus is per-source **on purpose**: the same program can have
  different expected bytes in different implementations. jaq deliberately
  diverges from jq in places (e.g. `[(1,2) * (3,4)]` is `[3,4,6,8]` in jaq,
  `[3,6,4,8]` in jq), error messages and formatting differ everywhere, and
  `!error` cases pin nothing. Never merge the `cases/` files into one
  expectation set; compare per source.
- The `sources/` files are pristine — when a normalized case looks wrong,
  the upstream file is the authority.
- Cross-tool smoke check used while building: `jq --run-tests` on
  `cases/jq-1.8.2.test` passes 538/550 and on `cases/jqlang-jq.test`
  1280/1359 with jq-1.8.2 (the misses are jq-vs-jq drift: post-1.8.2
  features, decimal-number conditionals, runner quirks — not format
  errors). `jqjq.test` passes 174/179 (some cases use gojq/jqjq-only
  builtins like `group`).

## What was searched and excluded

GitHub search turned up no dedicated "jq expression" corpus repos; the
useful corpora are the implementations' own test suites, which is what this
directory gathers. Evaluated and rejected: **jql** (yamafaktory/jql — a
different dialect, tests would not run on jq), **fq** (wader/fq —
jq-superset with fq-only builtins in its `pkg/interp/*.jq.test`), and
**TinyJQ** (MichalStrehovsky — repository deleted; only an empty fork
remains). jq's `tests/shtest` is included as a file but not parsed (shell
harness).

## Regeneration

```sh
./fetch.sh            # re-fetch pinned commits into /tmp/jq-corpora-src, rebuild
./fetch.sh --latest   # move all checkouts to upstream HEAD, rebuild
```

`extract.py` is idempotent and only reads the checkouts. After `--latest`,
review and update `manifest.json`'s commits and this README's table.
