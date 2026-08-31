# jqf-codec-core

Format-neutral codec contracts for jqf: registration, access sessions, encode
sinks, record streams, and the failure vocabulary.

This crate is `no_std` and uses `alloc`. It depends on `jqf-source` for spans
and diagnostics, `jqf-resource` for the work budget and cancel, and `jqf-data`
for documents and values. It contains no parser.

What it has:

- `CodecRegistration` / `CodecDescriptor` — validated format + dialect + factory
  records
- `ErasedProvider` / `ErasedAccessSession` — bind and decode
- `ErasedEncoderFactory` / `ErasedEncoderSession` / `ByteSink` — encode
- `ErasedRecordStreamProvider` / `RecordItem` / `RecordBatch` — framed records
- `CodecError` / `CodecFailureKind` — closed failure vocabulary
- `byte_scan::prefix_len` — stop-set SIMD prefix scan
- `PruneTree`, `PruneLookup`, `AccessFootprint`, comment roles, kernel markup
  segments

## Routes and failures

```rust
use jqf_codec_core::{CodecError, CodecFailureKind, PhysicalRouteId};

let route = PhysicalRouteId::derive("json", 1, 0).unwrap();
assert_ne!(route.get(), 0);

let error = CodecError::new(CodecFailureKind::InvalidInput);
assert_eq!(
    error.to_string(),
    "the input does not match the selected format or dialect"
);
```

## Bind

The existing binder is the Exact-binding interface — no new trait. Bind is
first-match: Direct `None`, else the CompleteDocumentExact family, else
CompleteDocumentDemand; demand clauses refuse at bind (`HardMismatch`). Direct
Exact republishes the selection as product root; CompleteDocumentExact fallback
keeps the full graph and names the child. Document codecs advertise a Direct
Exact slot; attribute/markup clauses that slot cannot serve take
CompleteDocumentExact. Record inventories never appear in access bind. See
[`CONTRACTS.md`](../CONTRACTS.md) for the ladder and product-shape law.

## Contracts

See [`CONTRACTS.md`](../CONTRACTS.md) for membership, lifecycle, and add-a-codec
invariants.
