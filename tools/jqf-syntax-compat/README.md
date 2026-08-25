# jqf syntax compatibility harness

This repository tool compares `jqf-syntax` grammar acceptance with an exact
`jq-1.8.2` executable. It is syntax-only: jq exit 3 is a compile rejection,
while successful compilation and runtime exit 5 are accepted syntax. Fixtures
avoid undefined functions and invalid assignment targets because jq reports
those semantic compile failures with the same process status as grammar errors.

**Manual-run only.** The tool is deliberately not a standing gate lane: the
oracle must be exactly `jq-1.8.2` on this machine, which no gate tier can
assume. Run it explicitly after touching the grammar surface; registering it
as a lane means first committing the pinned oracle binary or a fetch step for
one.

```console
cargo run -p jqf-syntax-compat -- --jq /path/to/jq-1.8.2
```

The oracle path is selected from `--jq`, then `JQF_JQ_ORACLE`, then
`tools/jq-1.8.2`. Any version output other than exactly `jq-1.8.2` is refused.
`jaq` and other jq-shaped implementations are never accepted as the oracle.
The `let`, `.@`, and `.&` fixtures are checked only with jqf and are never sent
to jq.

The version and process-status mechanics are adapted from the historical
`.jqf-old-base/tools/jq_oracle.py`; the focused syntax cases follow the shape of
`.jqf-old-base/tests/oracle/syntax`. Those paths are attribution references,
not active authority.
