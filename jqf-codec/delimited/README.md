# jqf-codec-delimited

CSV (RFC 4180) and TSV record-stream framing for jqf.

This crate is `no_std` and uses `alloc`. It depends on `jqf-codec-core`,
`jqf-data`, `jqf-resource`, and `jqf-source`. It owns PHYSICAL framing for
record streams whose payloads are RFC 4180 delimited records: record
boundaries, ordinals, terminators, and the quote-aware framing law. It owns no
field grammar — field splitting, quoting, and the header row belong to the
payload codec reached by narrowing the same retained source to the record's
byte range.

What it has:

- `registration` / `registration_tsv` — catalog entries (`csv`, `tsv`)
- the record route, the adjacent-value input model, and the source-preserving
  edit lane
- the two CSV input families: `csv.rfc4180@1` (the frozen RFC alphabet) and
  `csv.utf8@1` (the Unicode-capable sibling), each with a headered dialect
- `FORMAT_ID` and the dialect / route id constants

```rust
use jqf_codec_delimited::{FORMAT_ID, registration};

let registration = registration().unwrap();
assert_eq!(registration.descriptor().format().as_str(), FORMAT_ID);
```

Family laws: [`CONTRACTS.md`](../CONTRACTS.md).
