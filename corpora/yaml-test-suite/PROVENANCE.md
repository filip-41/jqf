# Vendored YAML test suite

`src/<ID>.yaml` is the yaml-test-suite corpus, copied verbatim from the
upstream repository at the pinned commit. It is the input to
`tools/jqf-codec-yaml-differential`.

It is vendored rather than fetched at run time on purpose: a standing gate that
needs the network is a gate that fails for reasons unrelated to jqf. Nothing in
this directory is edited — a divergence from the upstream corpus belongs in the
harness's allowlist, never in the oracle.

| item | value |
| --- | --- |
| upstream | `https://github.com/yaml/yaml-test-suite` |
| commit | `da267a5c4782e7361e82889e76c0dc7df0e1e870` |
| tree sha256 | `ec16dffd2ff84ebc155cc92fd4a207135258580911ecdd1672bf4228968722af` |

The corpus carries 351 cases: each names a `yaml:` input, a `tree:` event
graph, optionally a `json:` canonical projection, and 82 are marked
`fail: true` (the decoder must REJECT them). The `json:` projections are the
decode oracle; the `dump:` fields (where present) are the encode oracle.

yaml-test-suite is distributed under the MIT license; see
`https://github.com/yaml/yaml-test-suite`.

## Updating

Vendor the `src/` directory at a new upstream commit, record the commit and
the tree sha256 above, and re-pin the differential receipt with the cause
named — a receipt whose value MOVES is re-pinned in the same commit exactly
as with every fuzz receipt.
