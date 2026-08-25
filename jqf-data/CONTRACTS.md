# jqf-data Contracts

Invariants for this crate and for hosts. Type overview and examples live
in [README.md](README.md).

`jqf-data` is owned values and the immutable document they come from. It
does not parse or encode documents, and it does not evaluate programs.
It does parse RFC 3339 text and number literals. A host builds values
and documents here, then reads them.

It depends on `jqf-source` and `jqf-resource`. `triomphe` backs shared
numbers, aggregates, and document storage. Source seals are metadata
fingerprints (identity, base offset, byte length); they carry no content
digest — the caller's documented ownership of the exact immutable segment
is the integrity law.

## Public types

The crate root re-exports one closed surface; `lib.rs` is the inventory.

Value: `Value`, `Array`, `Object`, `ObjectBuilder`, `ObjectEntry`,
`ObjectKey`, `Shared`, `TagId`, `TagError`,
`ValueAllocationError`, `ValueKind`, `resolve_index`, and the borrowed
views and iterators (`ValueView`, `ScalarView`, `ArrayView`, `ArrayIter`,
`ObjectView`, `ObjectIter`, `ObjectEntryView`, `NumberView`,
`OffsetDateTimeView`, `LocalDateTimeView`, `LocalTimeView`).

Numbers: `Number`, `Integer`, `Decimal`, `Float`, `BigInt`,
`NumberCategory`, `NumericError`, `DecimalText`, `decimal_parts_to_f64`,
and `format_binary64` (returns the crate-internal `Binary64Text`; callers
read its text without naming the type).

Temporal: `OffsetDateTime`, `LocalDateTime`, `LocalDate`, `LocalTime`,
`FractionalSecond`, `UtcOffset`, `KnownUtcOffset`, `TemporalError`,
`parse_rfc3339`, `write_epoch_rfc3339`, `temporal_to_epoch`,
`days_from_civil`, `civil_from_days`, `civil_from_epoch`,
`epoch_seconds_from_civil_parts`, `try_epoch_seconds_from_civil_parts`.

Document building: `Document`, `DocumentId`, `NodeHandle`, `NodeId`,
`OccurrenceId`, `FactId`, `AccountedDocumentBuilder`,
`AccountedSemanticNode`, `AccountedIntrinsicTag`, `AccountedOccurrenceKey`,
`AccountedTextStage`, `PreparedSemanticNode`, `DocumentCapacity`,
`DocumentTransients`, `AccountedDocumentFinalizer`,
`DocumentFinalizationPoll`, `SliceRange`, `LocalOwnerRef`, `ExpandedName`,
`ExpandedNameError`.

Schemas and coverage: `DocumentSchemaRecipe`, `DocumentSchemaPrototype`,
`PreparedDocumentSchema`, `PreparedNodeKind`, `PreparedOccurrenceRole`,
`FactRoleBindingId`, `FactKindBindingId`, `BuilderCoverage`,
`AuthoritativeEmptyFamilies`, `DiagnosticCoverage`, `DocumentCoverage`,
`DocumentCapability`, `DocumentCapabilityFamily`, `FormatId`, `DialectId`,
`FormatIdRef`, `DialectIdRef`, `FormatIdError`.

Facts and tags: `DocumentFact`, `FactPayload`, `FactPayloadList`,
`FactPayloadMap`, `FactPayloadView`, `FactRoleId`, `OccurrenceRoleId`,
`FactKindId`, `DocumentNodeKindId`, `NamespacedIdError`, `IntrinsicTag`,
`IntrinsicTagSemantics`, `ContainerSpanKind`.

Source: `DocumentSourceText`, `DocumentSourceBinding`,
`DocumentSourceBindingStage`, `DocumentSourceBindingPoll`,
`DocumentTextId`, `DocumentTextStorageStats`.

Counts and elements: `CountDemand`, `CountStep`, `CountFilter`,
`CountLiteral`, `CountMember`, `CountRow`, `CountCompare`, `CountTest`,
`CountVerdict`, `ElementDemand`, `ElementProbe`, `ElementRow`,
`ElementVerdict`, `owned_probe_value`.

Readers and materialization: `TopologyReader`, `TopologyBatch`,
`NodeBatch`, `NodeIter`, `OccurrenceBatch`, `OccurrenceIter`,
`DocumentNodeView`, `OccurrenceView`, `FactReader`, `FactBatch`,
`BatchLimit`, `ReaderPoll`, `ReaderCompletion`, `ReaderDemand`,
`MaterializeWorkspace`, `LazySpanMaterializer`, and `DataError`.

Under the non-default `benchmark-internals` feature,
`DocumentStorageLayoutStats` also joins this surface (`#[doc(hidden)]`).

Adding a public item to `lib.rs` without a matching line here is an
incomplete change.

## Value

- `Value` is the owned semantic value.
- It has no `PartialEq`, `Hash`, or `Ord`. The caller that compares
  values owns that comparison.
- Numbers are integer, exact finite decimal, or binary64.
- Arrays keep item order.
- Objects keep unique UTF-8 keys in first-insertion order.
- `ObjectBuilder` keeps the first key position and the last value.
- Every heap payload lives behind one `Shared` handle. `Clone` bumps the
  refcount and copies nothing.
- A later write goes through `try_*_mut`, which copies a shared
  allocation before writing. A shared clone then looks the same as a
  separately built twin.
- `Value` is `Send`. Heap payloads are refcounted; they do not hold a
  request-ledger residency.

## Value allocation

- Construction is fallible if the allocator refuses. It does not charge
  a request ledger. Value, array, and object constructors take no
  resource context.
- `Clone` is a refcount bump. A later write copies the shared
  allocation first.
- `Number` and `FractionalSecond` construction are the same: fallible,
  uncharged, copies of digits the caller already has. `BigInt` is
  scratch arithmetic the caller drives.

## Tags

- `TagId` is one nonempty format-neutral string.
- `Value::Tagged` is the only owned non-core tag.
- A tag belongs to that one value. Keys and children do not inherit it.
- `Value::kind` and navigation look through the wrapper.
- Equality, hashing, and encoding still see the tag.
- `IntrinsicTag::core` must match the node's core category.
- `IntrinsicTag::tagged` materializes as `Value::Tagged`.
- The intrinsic tag is the one a fact-style tag read returns. An
  attached `DocumentFact` cannot replace it.
- This crate has no native tag domains, core-tag tables, or format
  representability rules.

## Shared helpers

These live here because they are about this crate's types. Callers
delegate instead of copying them:

- `resolve_index` turns a signed index into a position (`.[-1]` counts
  from the end; out of range is `None`).
- `days_from_civil` / `civil_from_days` are unclamped proleptic Gregorian
  math. `civil_from_epoch` is the clamped `0000..=9999` form RFC 3339
  writers use.
- `parse_rfc3339` is the RFC 3339 parser for the uppercase `T` / `Z`
  production; lowercase spellings are `Syntax` (deliberate narrowing,
  recorded 2026-08-21). `write_epoch_rfc3339` and `UtcOffset::write_text`
  produce the canonical text; the offset writer refuses a sub-minute
  offset of EITHER sign rather than truncate it into a different
  meaning. A format with its own stricter grammar (TOML, CBOR tag 0)
  checks first, then delegates.
- Each temporal kind owns its own canonical text. Write that text
  instead of rebuilding it from fields.

## Document

- `Document` is one immutable document identified by a process-local
  `DocumentId`. There is no revision dimension; a successor would mint a
  fresh document.
- Node, occurrence, and fact ids are dense and document-local.
- A handle checks the document id before resolving a local id.
- The only handle type is `NodeHandle`: a document id paired with a dense
  local id, minted by `node_handle` and resolved by `resolve_node_handle`.
  Facts resolve by bare `FactId` through `Document::fact`; occurrence ids
  stay bare too.
- Occurrence topology may keep duplicate keys and shared or cyclic
  edges. Semantic object projection still keeps the first key position
  and the last value.
- Attached facts are ordered portable metadata. They cannot change
  intrinsic meaning. A fact may carry an authored source span (markup
  attribute quoted-value ranges) that node-keyed `authored_spans` cannot
  hold; `DocumentFact::source_span` reads it.
- `AccountedDocumentBuilder::record_fact_authored_span` binds that span
  under the same source-seal law as `record_authored_span`.
- `AccountedDocumentBuilder::try_reserve` may keep newly acquired
  capacity after an allocation failure. It never changes content, ids,
  or order.

## Source

- Source identity is `SourceRef`. Positions are compact
  segment-relative `Span` values from `jqf-source`.
- `DocumentSourceBinding` seals one complete immutable source segment.
- `DocumentSourceBinding::from_resolved` hashes already-resident bytes
  without charging the request ledger; codecs must use
  `DocumentSourceBindingStage`.
- `DocumentSourceText` is created only after bounds and UTF-8
  validation.
- Source bytes reach callers through `Document::source_segment`. The
  document carries no borrowed-source lookup surface; the decoder session
  owns identity and bounds.

## Readers and materialization

- Two readers: `TopologyReader` (nodes, then ordered occurrences) and
  `FactReader` (attached portable facts).
- A reader batch is bounded by a caller's `BatchLimit` and by work
  admission from `jqf-resource`. There is no caller-chosen byte budget.
- Every visible batch or completion checks cooperative control first.
- A reader that fails is terminal.
- Materializing a node with no value projection fails. It never skips
  the node silently.
- Completion evidence is bound to the document id and which reader
  finished. A `ReaderCompletion` carries the process-local `DocumentId`
  plus a `ReaderDemand` naming the finished reader (`Topology` or
  `Facts`); both read back through accessors, and nothing else is bound
  to it.
- Materialization is iterative and allocation-fallible.
- The `MaterializeWorkspace` object-key reuse cache is the one admitted
  uncharged residency: a small fixed bound of short, repeated object-key
  text retained across walks so a record shape interns its field names
  once. Every materialized value itself stays fully accounted; only this
  cache escapes the ledger.
- An active-path semantic cycle fails rather than recurse.
- Core intrinsic tags stay document facts. Non-core tags survive as
  `Value::Tagged`.
- Materializing an object walks the unique semantic projection
  (first position, last value). Older raw occurrences stay topology.
- Materialization produces an owned value only. It does not create a
  successor document.

## Errors

Every public fallible API returns one of these closed types. All
implement `Display` and `core::error::Error` and are `no_std`.

- `DataError` — document construction, reader, handle, and
  materialization failures (`CapabilityUnavailable`,
  `ContradictoryCoverage`, `InvalidDocument`, cycle, unrepresentable,
  and others). Also wraps `Resource(ResourceError)` and
  `Control(ControlError)`. Clone, Copy, Eq; `non_exhaustive`.
- `ValueAllocationError` — value storage failures. Unit struct; Clone,
  Copy, Eq; `non_exhaustive`.
- `TagError`, `FormatIdError`, `NamespacedIdError`, `ExpandedNameError`
  — identity validation. Clone, Copy, Eq.
- `NumericError` — number-grammar failures. `TemporalError` — RFC 3339
  syntax, year-range, and fraction failures. Both Clone, Copy, Eq.

`ResourceError` and `ControlError` are `jqf-resource` types. Match them
through `DataError::Resource` / `DataError::Control`.

## Panic and unsafe

Production code does not panic on user input. Fallible work returns an
error type. Allocations go through a fallible reservation.

`unsafe` is confined to checked-index reads and single-owner interior
mutation of shared backing (`document/schema.rs`, `value/shared.rs`,
`value/object/key.rs`) and to the builder's bound-span admission surface
(`document/builder.rs`: the `add_prepared_bound_*` constructors,
`record_authored_span`, `record_fact_authored_span`, and the call-local
source view they read through). The public unsafe entry points skip
re-validating a source a decoder session already proved:

- `AccountedDocumentFinalizer::poll_with_source`
- `DocumentSourceBinding::text_from_bound_authority`
- `Document::with_borrowed_source_from_bound_authority`
- the nine `pub unsafe fn` admission methods on `AccountedDocumentBuilder`
  (enumerated 2026-08-21 after the frozen-crate review found the
  inventory understated them): `consume_bound_stored_text_span`,
  `bound_stored_text`, `add_prepared_bound_container_span_node`,
  `add_prepared_bound_source_string_node`,
  `add_prepared_bound_source_integer_node`, `record_authored_span`,
  `record_fact_authored_span`, `add_prepared_bound_source_occurrence`,
  `add_prepared_bound_stored_occurrence`

All are `#[doc(hidden)]`. The caller must own the exact immutable
source segment the binding was sealed against, from sealing through the
call. Matching metadata is not enough. If you cannot prove that, use
the safe `poll` / construction paths.

## Tests

- `tests/all.rs` is the only integration target, over
  `tests/cases/value.rs` and `tests/cases/document.rs`.
- The `lib.rs` doctest is the compile-checked construction example.
- The four `examples/` programs (`values`, `document`, `numbers`,
  `temporal`) compile under `--all-targets`.

## How this contract changes

- Adding a public item to `lib.rs` or a variant to an error type
  updates the Public types / Errors sections in the same commit.

## Exclusions

This crate owns no format grammar, native parser storage, tag
interpretation, collision checking, rendering, edit planning, record
framing, query operator, execution plan, filesystem behavior, or host
framing. Native record ordinals, predecessor cache policy, and
canonical byte encodings for tag identities also live elsewhere.
