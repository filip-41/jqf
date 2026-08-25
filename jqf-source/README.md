# jqf-source

Defines byte ranges, source identity, and structured diagnostic records.

What it has:

- `Span` — `[start, end)` byte range in one source
- `SpanError` — `start > end`, or an offset bigger than `u32`
- `SourceFileRange` — one file's range inside a concatenated input
- `SourceId`, `SourceKind`, `SourceRef` — which source a label points at
- `ResolvedSource` — borrowed source bytes retained by a caller
- `Namespace` / `Code` — `namespace.name` diagnostic ids
- `Diagnostic`, `DiagnosticSource`, `Label`, `Severity`, `LabelStyle` — diagnostic components

## Source kinds

The same numeric id can name two sources:

- `Query` is the program
- `Input` is the document being processed

```rust
use jqf_source::{SourceId, SourceKind, SourceRef, Span};

let program = SourceRef::new(SourceId::new(0), SourceKind::Query);
let document = SourceRef::new(SourceId::new(0), SourceKind::Input);
assert_eq!(format!("{program}"), "query#0");
assert_eq!(format!("{document}"), "input#0");

let span = Span::new(0, 4);
assert_eq!(span.to_string(), "0..4");
```

## Spans

`Span` stores `[start, end)` byte offsets as `u32`.

```rust
use jqf_source::Span;

let source = "alpha\nbeta";
let span = Span::from_usize(6, 10);
assert_eq!(&source[span.range()], "beta");
assert_eq!(span.to_string(), "6..10");
assert!(Span::try_new(4, 2).is_none());
```

## Source identity

`SourceRef` combines a `SourceId` with a `SourceKind`. Use the full `SourceRef`
when retaining source bytes.

```rust
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

let source = SourceRef::new(SourceId::new(7), SourceKind::Input);
let resolved = ResolvedSource::new(source, "stdin", b"true", 0);
assert_eq!(resolved.bytes(), b"true");
```

## Diagnostic codes

`Namespace` names the static producer of a code. `Code` renders as
`namespace.name`.

Namespaces are non-empty lowercase ASCII segments. Code names are non-empty
dot-separated segments. Segments may contain `a-z`, `0-9`, `_`, and `-`.

```rust
use jqf_source::Namespace;

const SOURCE: Namespace = Namespace::new("source");
let code = SOURCE.code("invalid-span");

assert_eq!(code.to_string(), "source.invalid-span");
```

## Source metadata

`DiagnosticSource` keeps a name and a base offset for a `SourceRef` so
labels can stay relative to that segment. Sources and labels stay in the
order they were added.

If the same `SourceRef` appears twice, a renderer can use the first one.
Add `base_offset` to a label span when you need an absolute position.
While the bytes are still around, `ResolvedSource` has the same offset.

## Contracts

See [`CONTRACTS.md`](CONTRACTS.md) for source, span, and diagnostic invariants.
