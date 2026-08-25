# jqf-data

Owned values and the immutable document they come from.

This crate is `no_std` and uses `alloc`. It depends on `jqf-source` for
spans and source identity, and on `jqf-resource` for the work budget and
cancel. It does not parse or encode documents, and it does not evaluate
programs. It does parse RFC 3339 text and JSON/YAML number literals.

What it has:

- `Value` — owned semantic value: null, bool, number, string, bytes,
  dates and times, a tagged wrapper, array, object
- `Document` — one immutable format-neutral document
- `Integer`, `Decimal`, `Float`, `Number`, `BigInt` — exact and binary
  numbers
- `OffsetDateTime`, `LocalDateTime`, `LocalDate`, `LocalTime` — dates
  and times, plus RFC 3339 parse and write
- borrowed views (`ValueView`, `ArrayView`, `ObjectView`, …)
- bounded readers (`TopologyReader`, `FactReader`)
- `AccountedDocumentBuilder` — build one document
- `resolve_index` — signed indexes (`.[-1]` counts from the end)
- civil-calendar helpers (`days_from_civil`, `civil_from_days`,
  `civil_from_epoch`)

## Tags

A tag is one nonempty string on exactly that value:

```rust
use jqf_data::{TagId, Value, ValueKind};

let tag = TagId::try_new_unaccounted("!layer").unwrap();
let value = Value::try_tagged(tag, Value::Bool(true)).unwrap();

assert_eq!(value.tag().map(TagId::as_str), Some("!layer"));
assert_eq!(value.kind(), ValueKind::Bool);
```

## Signed indexes

Signed indexes count from the end:

```rust
use jqf_data::resolve_index;

let items = ["a", "b", "c"];
let position = resolve_index(items.len(), -1).unwrap();

assert_eq!(items[position], "c");
assert_eq!(resolve_index(items.len(), 3), None);
```

`Value` has no `PartialEq`, `Hash`, or `Ord`; comparison lives with the
caller that owns what equal means. `kind()` looks through a tag
wrapper. A `Document` keeps source order and duplicate keys in its
topology, then projects one semantic object: first key position, last
value. Value and number construction fail only if the allocator refuses
and never charge a request ledger.

## Contracts

See [`CONTRACTS.md`](CONTRACTS.md) for value and document invariants.
