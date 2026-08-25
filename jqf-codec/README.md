# jqf-codec

One format-neutral contract crate plus one crate per format. `core` owns
registration, requests, capabilities, erased sessions, and the failure
vocabulary — and contains no parser.

| Crate | Format(s) | Notes |
|---|---|---|
| `core` | — | contracts only; the one crate every consumer reaches |
| `json` | JSON (RFC 8259), JSONC, JSON5, NDJSON, json-seq (RFC 7464) | the full-capability reference codec; `--edit` lane |
| `delimited` | CSV/TSV (RFC 4180 dialects) | record streams |
| `toml` | TOML 1.0 / 1.1 | |
| `yaml` | YAML (core/json/failsafe dialects) | multi-doc via adjacent values |
| `cbor` | CBOR (RFC 8949), CBOR sequences (RFC 8742) | binary |
| `xml` | XML | markup projection |
| `html` | HTML (+ fragment dialect) | error-correcting parse |
| `ini` | Java properties / INI / dotenv | text config family |
| `messagepack` | MessagePack | binary; `--edit` |
| `jqft` | jqft / jqfjson / jqfb | internal native text and binary images |
| `render` | rendered output | encode-only |

Surface and examples: [core/README.md](core/README.md). Invariants:
[CONTRACTS.md](CONTRACTS.md).
