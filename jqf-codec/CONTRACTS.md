# jqf-codec Contracts

Invariants for this family and for hosts. Type overview and examples live in
[core/README.md](core/README.md).

`jqf-codec-core` is format-neutral contracts. It does not parse or encode a
format. Each format crate owns one format family.

## Membership

An item belongs in core when both hold:

- it is grammar-free, or a non-codec must name it without importing a format
- it is not a format's schema, wire identity, or indent/escape grammar

Stay: erased access/encode/record ABI, stop-set `prefix_len` and UTF-8 lane
kernels, decimal render, comment positions, kernel markup segments,
prune/pattern/product/project, record format id strings.

Leave: JSON string escape, format schema strings (`xml.element@1`), record
option structs.

## Accounting

Every allocation core retains is charged on the request account. A site that can
grow without bound calls `try_reserve`.

## Bind and lifecycle

Bind is first-match over advertised routes, then adapters, then the
whole-document demand fallback.

The bind ladder: Direct `None` → CompleteDocumentExact family →
CompleteDocumentDemand → HardMismatch.

Exact with no Exact slot opens Whole + `CompleteDocumentExact`. Product is still
Located (named child). That is the bind ladder (attribute/markup fallback), not
codecs ignoring Exact. Document codecs bind Direct Exact (slot 1). Record
framers are Whole by physics.

Range footprints refuse every core adapter (NO-CORE-FALLBACK).

Direct Exact publishes the selection as product root. Fallback names the child.
`node == root` is not Exact vs Whole.

Record inventories never appear in access bind.

Demand clauses bind: a clause no route can serve is
`AccessBindError::HardMismatch`. Result authority is a hint and may fall back to
the lazy whole document.

Open seals coverage, plan, and route receipt once. Decode is one straight-line
call. Impossible states are `InternalContractViolation`, never a panic.

## Publish

Nothing is published until an item completes. The validating scan visits every
byte before any node is published. Deferral changes whether a node is built, not
whether a byte is validated.

## Erasure

`ErasedProvider`, `ErasedAccessSession`, `ErasedEncoderFactory`,
`ErasedEncoderSession`, and `ErasedTagValidator` are `Box<dyn Trait>` newtypes.
A wrong concrete type does not compile.

## Hints

`CodecDemand` clauses bind. Delivering more than asked is sound. Catalogued
`.@` attached-fact identities are advertised so bind does not hard-mismatch.
`.&name` attributes are not advertised; JSON binds them through absence and
markup formats through the whole-document / `CompleteDocumentExact` fallback. Do
not invent an Exact+Attribute footprint.

Shortcut oracles (count / keys / type / has / element / min-max *answers*) are
not this crate. Codecs never see a shortcut job. After locate, the engine asks
`Document::count_children_from` / `visit_elements_from`. `AccessRequirement` is
bind/decode shape plus monotone hints.
The bind ladder is [Bind and lifecycle](#bind-and-lifecycle).

## Reuse

Recycled session and encoder state equals fresh state. The residual cache is
keyed by `ResidualKey`, which fingerprints prune.

## Record streams

Framed byte ranges, never documents. Batches are caller-owned.
`RecordItem::try_new` rejects incoherent extents.

## Errors

`CodecFailureKind` is closed. User-reachable renderings are prose, never Rust
syntax. `RawNulByte` is the only per-value recoverable kind.

## byte_scan

`prefix_len` is the stop-set scan. UTF-8 lane kernels live here; the windowed
walk lives with the JSON crate. SIMD `unsafe` is in this module. `product`
(borrowed-source attach) and `erased` (fallible box) also contain `unsafe`.

## Adding a codec

One crate on `jqf-codec-core`, `jqf-data`, `jqf-resource`, and `jqf-source`.
Implement `InputProvider` or `RecordStreamProvider`. Encode through
`EncoderFactoryImpl` and `EncoderSession`. Export `registration()`.

Access slots are the provider's `route_descriptions()`, not the descriptor's CLI
facts. The CLI facts are `Record`, `AdjacentValues`, and `Edit`.

`--edit` needs span binding, an edit-render dialect, a splice policy, and
`RouteCapability::Edit`. Alias-ambiguous nodes use `EDIT_REFUSAL_ROLE`.
Merge-inherited members use `MERGE_OVERRIDE_ROLE`.
