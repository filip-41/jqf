# jqf-syntax

Lexer, parser, and source-preserving syntax trees for jqf programs.

This crate is `no_std` and uses `alloc`. It depends on `jqf-source` for
spans and source identity. It does not resolve names, load modules, or
evaluate programs.

What it has:

- `parse_query` / `parse_program` / `parse_library` — parse one source unit
- `Lexer` / `Token` / `TokenKind` — tokens with byte spans
- `Expr`, `Pattern`, `SourceUnit` — source-preserving trees
- `SyntaxNodeKind` — the closed authored-form inventory
- `ParsedSyntax::bind` — attach retained source bytes to a tree
- `let`, `~name` / `~name(args)` / `as ~x` — jqf surface
- contiguous `.@` and `.&` postfixes — node/value and markup-attribute access

Public entry points refuse source longer than `MAX_SYNTAX_SOURCE_BYTES`
before lexing, and refuse nesting past `MAX_SYNTAX_NESTING_DEPTH` during
the parse.

## Parse a query

Every public span is a half-open UTF-8 byte range in the supplied source.
Use `into_valid_syntax` at lowering or execution so a recovery tree cannot
be mistaken for valid syntax.

```rust
use jqf_source::{SourceId, SourceKind, SourceRef};
use jqf_syntax::{ExprKind, parse_query};

let text = ".price.@tag // \"untagged\"";
let source = SourceRef::new(SourceId::new(1), SourceKind::Query);
let syntax = parse_query(source, text)
    .unwrap()
    .into_valid_syntax()
    .unwrap();
assert_eq!(syntax.source_ref(), source);
let expression = syntax.root();

assert_eq!(&text[expression.span().range()], text);
let ExprKind::Binary(binary) = expression.kind() else {
    panic!("expected the alternative operator");
};
assert_eq!(&text[binary.op_span.range()], "//");
```

Recovery stays available for editors and diagnostics:

```rust
# use jqf_source::{SourceId, SourceKind, SourceRef};
# use jqf_syntax::parse_query;
# let source = SourceRef::new(SourceId::new(1), SourceKind::Query);
let parsed = parse_query(source, ". @tag").unwrap();
assert!(!parsed.is_valid());
assert!(!parsed.diagnostics().is_empty());
assert!(parsed.into_valid_syntax().is_err());
```

## Bind retained source text

Trees keep source identity and byte length, not the text. Bind a tree to
resolver-owned bytes when consuming its spans.

```rust
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};
use jqf_syntax::parse_query;

let text = r#""value=\(.price.@tag)""#;
let source = SourceRef::new(SourceId::new(4), SourceKind::Query);
let syntax = parse_query(source, text).unwrap().into_valid_syntax().unwrap();
let bound = syntax
    .bind(ResolvedSource::new(source, "query", text.as_bytes(), 0))
    .unwrap();

assert_eq!(bound.source().text(), text);
assert_eq!(bound.root().span().range(), 0..text.len());
```

## Parse source units

Programs keep declaration order and punctuation spans. Libraries use the
same declarations but may omit the final query.

```rust
use jqf_source::{SourceId, SourceKind, SourceRef};
use jqf_syntax::{SourceItem, parse_program};

let text = "include \"strings\"; def twice(f): f | f; twice(.)";
let source = SourceRef::new(SourceId::new(2), SourceKind::Query);
let unit = parse_program(source, text)
    .unwrap()
    .into_valid_syntax()
    .unwrap();

assert!(matches!(unit.root().items[0], SourceItem::Include(_)));
assert!(matches!(unit.root().items[1], SourceItem::Def(_)));
assert!(unit.root().expression.is_some());
```

See [`SYNTAX.md`](SYNTAX.md) for the language target.

## Contracts

See [`CONTRACTS.md`](CONTRACTS.md) for parse, recovery, and inventory
invariants.
