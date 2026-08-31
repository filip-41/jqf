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
- borrowed views (`ValueView`, `ArrayView`, `ObjectView`, `ScalarView`, …);
  `ScalarView` is the borrowed atom form of an owned `Value` or a document
  scalar (`NumberView` is `Number | Integer(&str) | Decimal | Float`).
  There is no `Atom` type
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

## Reservation and workspace reuse

`DocumentCapacity` plus `try_reserve` is a builder hint for nodes,
occurrences, stored text, and facts. It does not reserve `wide`,
`tags`, or per-role `owner_positions`. A missed hint still builds; it
is not a completeness claim.

`MaterializeWorkspace` reuses cycle-detection scratch across
`materialize_root_with` / `materialize_node_with`. The one-shot
`materialize_root` / `materialize_node` paths allocate a fresh
workspace each time.

```rust
use jqf_data::{
    AccountedDocumentBuilder, AccountedSemanticNode, DocumentCapacity, MaterializeWorkspace, Value,
};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

static CONTROL: ContinueControl = ContinueControl;
let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
let mut resources = ResourceContext::new(
    RequestAccount::try_new(limits)?,
    &CONTROL,
    WorkMeter::try_new_v1(1).ok_or("work meter")?,
)?;

let mut builder = AccountedDocumentBuilder::try_new("example", None)?;
builder.try_reserve(
    DocumentCapacity {
        nodes: 1,
        ..DocumentCapacity::default()
    },
    &resources,
)?;
let root = builder.add_node("example.bool", AccountedSemanticNode::Bool(true), None, &resources)?;
let document = builder.finish(root, &resources)?;
let mut workspace = MaterializeWorkspace::new();
assert!(matches!(
    document.materialize_root_with(&mut workspace, &mut resources)?,
    Value::Bool(true)
));
# Ok::<(), Box<dyn std::error::Error>>(())
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
wrapper. `Value` is `Clone` (a refcount bump). `Document` sharing is
`try_clone` / `Clone` (Arc tables, not a deep copy). A `Document` keeps
source order and duplicate keys in its topology, then projects one
semantic object: first key position, last value. Value and number
construction fail only if the allocator refuses and never charge a
request ledger. Document building charges a resource context.

## Contracts

See [`CONTRACTS.md`](CONTRACTS.md) for value and document invariants.
