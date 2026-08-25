# jqf-source Contracts

Invariants for this crate and for hosts. Type overview and examples live
in [README.md](README.md).

This crate does not read files, parse programs, decode documents, render
diagnostics, or check that a span falls inside retained bytes.

## Span

- Offsets are bytes stored as `u32`, relative to one
  segment.
- A `Span` does not name a source. Pair it with `SourceRef`.
- `try_new` / `try_from_usize` report failure. `new` / `from_usize` panic
  with the matching `SpanError` `Display` text.
- `try_from_usize` checks `start > end` on the `usize` pair first.
  `OffsetOverflow` is reported only for an ordered range that does not
  fit in `u32`.
- `merge` is the smallest span covering both inputs, gap included.
- Zero-width spans (`start == end`) are valid.

## Source identity

- Equality and lookup use the whole `SourceRef`.
- There is no third `SourceKind`. Streaming records are `Input` with a
  per-record `base_offset`.
- `ResolvedSource::base_offset` is defined for empty slices.

## SourceFileRange

Not a `Span`: `u64` offsets plus a filename, stored as given (no
`start <= end` check). The host keeps ranges contiguous, covering the
retained slice in argument order, joined with no separator. A value is
attributed to the file that contains its last byte.

## Diagnostics

- Source metadata and labels preserve insertion order, including
  duplicate `SourceRef`s. A renderer may use the first source record
  that matches a `SourceRef`.
- `DiagnosticSource` owns the display label and base offset. It does not
  retain bytes (`ResolvedSource` does).
- `try_*` constructors return `None` when the allocator refuses. The
  infallible constructors abort on OOM. This crate does not exercise
  the refusal path.
- `Severity`, `SourceKind`, and `LabelStyle` are exhaustive: a new
  variant is a breaking change.
- Codes are `'static` strings. `Namespace::new` / `Namespace::code` panic
  on a spelling that fails this grammar:

```text
namespace = segment
code-name = segment ("." segment)*
segment = 1*( "a".."z" | "0".."9" | "_" | "-" )
rendered-code = namespace "." code-name
```

`Code::namespace()` / `Code::name()` / `Namespace::name()` split a rendered
code without re-parsing it.
