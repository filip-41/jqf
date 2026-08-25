# Vendored jq test suites

**Source project: jq (`https://github.com/jqlang/jq`), MIT licensed.** Every
file under `jq-<version>/` is copied VERBATIM from the jq source tree at the tag
matching the jq version the directory is named for. They are the input to
`tools/jqf-jq-test-suite.py`, which selects one with `--suite`.

- **`jq.test`** — jq's own general test suite.
- **`onig.test`** — jq's oniguruma-derived REGEX suite, the material jq's
  `tests/` keeps apart from `jq.test` because it exercises the regex family
  specifically. Vendored for ledger 043's W6 as the REFEREE for the two-tier
  regex engine: `jq.test` carries almost no regex case, so before this file
  there was nothing standing to measure the regex family's divergences against.
  Its residual divergences are enumerated in
  `.docs-intenal/regex-divergence-catalogue-2026-08-04.md` and waived, one
  line each, in the harness's `ONIG_ALLOWLIST`.

They are vendored rather than fetched at run time on purpose: a standing gate
that needs the network is a gate that fails for reasons unrelated to jqf.
Nothing in this directory is edited — an oracle with a local edit is not an
oracle, so a divergence from jq belongs in the harness's allowlist and in the
catalogue, never in the file. That is also why the crediting note lives HERE
rather than in a header inside the file: the file has no room for one that
would not change its bytes.

| file | upstream | sha256 |
| --- | --- | --- |
| `jq-1.8.2/jq.test` | `https://raw.githubusercontent.com/jqlang/jq/jq-1.8.2/tests/jq.test` | `329689763b651096989bd8260b643731083fc5fd17f6bd7834d158713f738cbd` |
| `jq-1.8.2/onig.test` | `https://raw.githubusercontent.com/jqlang/jq/jq-1.8.2/tests/onig.test` | `e82dab356709d4a5e4dfd8c71aced12ed1f42eb23208bac0eaf5e3f05bedef05` |

## Adding a version

Vendor the file at the tag matching the jq the machine has on `PATH`, record its
sha256 above, and point `SUITE_VERSION` in `tools/jqf-jq-test-suite.py` at it.
The harness verifies the vendored file against that sha256 before a single case
runs, because a modified oracle is not an oracle; the installed jq's version is
informational only, since the harness never executes jq.
