# jqf-codec-core

Format-neutral codec contracts for jqf: registration, access sessions,
encode sinks, record streams, and the failure vocabulary.

This crate is `no_std` and uses `alloc`. It depends on `jqf-source` for
spans and diagnostics, `jqf-resource` for the work budget and cancel, and
`jqf-data` for documents and values. It contains no parser.

What it has:

- `CodecRegistration` / `CodecDescriptor` — validated format + dialect + factory records
- `ErasedProvider` / `ErasedAccessSession` — bind and decode
- `ErasedEncoderFactory` / `ErasedEncoderSession` / `ByteSink` — encode
- `ErasedRecordStreamProvider` / `RecordItem` / `RecordBatch` — framed records
- `CodecError` / `CodecFailureKind` — closed failure vocabulary
- `byte_scan::prefix_len` — stop-set SIMD prefix scan
- `PruneTree`, `PruneLookup`, `AccessFootprint`, comment roles, kernel markup segments

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

Demand clauses refuse at bind (`HardMismatch`). Result authority may fall
through to the whole-document adapter. See [`CONTRACTS.md`](../CONTRACTS.md).

## Contracts

See [`CONTRACTS.md`](../CONTRACTS.md) for membership, lifecycle, and
add-a-codec invariants.
