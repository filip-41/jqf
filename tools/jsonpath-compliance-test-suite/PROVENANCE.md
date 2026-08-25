# Vendored RFC 9535 Compliance Test Suite

**Source project: jsonpath-standard/jsonpath-compliance-test-suite
(`https://github.com/jsonpath-standard/jsonpath-compliance-test-suite`), MIT
licensed.** Every file below is copied VERBATIM from the source tree at the
pinned commit. They are the input to `tools/jqf-jsonpath-cts.py`, which runs
every case through the built `jqf` binary.

- **`cts.json`** — the consolidated Compliance Test Suite: 703 cases covering
  the RFC 9535 surface (root/child/descendant segments, name/index/slice/
  wildcard/filter selectors, unions, the five standard function extensions
  `length`/`count`/`match`/`search`/`value`, whitespace rules, and the
  well-formedness/validity rejections). Each case carries a `selector`, a
  `document`, and either a deterministic `result`+`result_paths`, an
  unordered `results`+`results_paths` (the spec allows several orders), or
  `invalid_selector: true` (the implementation MUST raise).
- **`cts.schema.json`** — the suite's own JSON Schema, documenting the case
  fields; used by the harness to check the vendored file's structure.
- **`LICENSE` / `NOTICE` / `README.md`** — the source project's license
  notices and its README.

They are vendored rather than fetched at run time on purpose: a standing gate
that needs the network is a gate that fails for reasons unrelated to jqf.
Nothing in this directory is edited — an oracle with a local edit is not an
oracle, so a divergence from the suite belongs in the harness's allowlist and
in the gate's receipt, never in the file.

| file | upstream | sha256 |
| --- | --- | --- |
| `cts.json` | `https://raw.githubusercontent.com/jsonpath-standard/jsonpath-compliance-test-suite/7be7c1fc28057c91e8eefaf197060fba7ed43acd/cts.json` | `a85db53fba1f675be48b534baec5a754dc685ad08c550d8927f609c7708f365a` |
| `cts.schema.json` | `.../7be7c1fc28057c91e8eefaf197060fba7ed43acd/cts.schema.json` | `4c6d539f94952a293c8be3cdc14dba31bb8d64ae43e08f0d19db86d54eb1c552` |
| `LICENSE` | `.../7be7c1fc28057c91e8eefaf197060fba7ed43acd/LICENSE` | `5a601add33771c5c4f43076a2113577efeddbd86f38969d4530ec9fed9336cee` |
| `NOTICE` | `.../7be7c1fc28057c91e8eefaf197060fba7ed43acd/NOTICE` | `f93a810bac07edd428aa0f27cf291dae8bf538306fb282439a4673ca929b408f` |
| `README.md` | `.../7be7c1fc28057c91e8eefaf197060fba7ed43acd/README.md` | `63939bb9b2100212c6113b71f96edaa8243e9eb21d9445f062b33bb785583a1f` |

The `LICENSE` and `NOTICE` files are vendored with line endings normalized to
LF (their upstream spellings use CRLF); the digest above is of the normalized
bytes as committed, so a fresh checkout verifies. `cts.json` is the digest the
harness enforces. The pinned commit is
`7be7c1fc28057c91e8eefaf197060fba7ed43acd` (2026-05-21).
