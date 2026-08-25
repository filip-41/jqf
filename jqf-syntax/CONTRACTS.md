# jqf-syntax Contracts

Invariants for this crate and for hosts. Type overview and examples live
in [README.md](README.md). The language target lives in
[SYNTAX.md](SYNTAX.md).

This crate does not resolve names, select builtins, load modules, evaluate
filters, interpret facts or attributes, or own codec behavior.

It depends on `jqf-source`.

## Public types

The crate root re-exports one closed surface; `lib.rs` is the inventory.

Parse: `parse_query`, `parse_program`, `parse_library`, `Parse`,
`ParsedSyntax`, `Lexer`.

Trees: `Expr`, `ExprKind`, `Pattern`, `PatternKind`, `SourceUnit`,
`SourceItem`, `DefItem`, `ImportItem`, `IncludeItem`, and the supporting
node types (`CallExpr`, `BindingExpr`, `ConditionalExpr`, …).

Tokens and operators: `Token`, `TokenKind`, `OperatorSpec`,
`Associativity`, `InfixOperation`, `BinaryOp`, `UnaryOp`, `AssignmentOp`.

Inventory and walk: `SyntaxNodeKind`, `SyntaxNodeRef`, `SyntaxWalk`,
`WalkEvent`.

Binding and decode: `BoundSyntax`, `SyntaxSource`, `SyntaxSourceError`,
`StringTemplate`, `TemplateSegment`, `decode_literal_into`,
`StringDecodeError`, `SyntaxViewError`.

Input and diagnostics: `MAX_SYNTAX_SOURCE_BYTES`,
`MAX_SYNTAX_NESTING_DEPTH`, `SyntaxInputError`, `SyntaxErrorKind`,
`ExpectedTokens`, `GrammarContext`.

Adding a public item to `lib.rs` without a matching line here is an
incomplete change.

## Input

- Public lexer construction and parse entry points reject source longer
  than `MAX_SYNTAX_SOURCE_BYTES` before lexing. That is a representation
  failure (`SyntaxInputError::SourceTooLarge`), not a recoverable grammar
  diagnostic.
- The parser refuses nesting past `MAX_SYNTAX_NESTING_DEPTH` (10 000)
  during the parse.
- Tokens and AST spans are half-open UTF-8 byte ranges in the supplied
  source.

## Parse and recovery

- Source items, control forms, bindings, collections, calls, patterns,
  operators, selectors, and optional postfixes keep their authored
  keyword, delimiter, separator, and operator spans.
- Recoverable parsing may return a syntax root together with diagnostics.
- Parser diagnostics keep a compact ordered `ExpectedTokens` set and
  `GrammarContext` where a grammar position has focused expectations.
  Unclosed delimiters and unterminated controls label both the missing
  component and the authored opener.
- Catchless `try expression` is complete syntax. Only an authored `catch`
  without a handler is diagnosed as unterminated.
- Recovery synchronization is owned by the enclosing grammar form. Calls,
  objects, patterns, groups/brackets, controls, and source items stop
  before their caller-owned separators or terminators; the caller consumes
  that token.
- Recovery either consumes at least one non-synchronization token or
  returns immediately at a caller-owned synchronization token. Missing
  punctuation uses a zero-width span at the current token start and never
  borrows the next authored token's span.
- `Parse::is_valid` requires both a syntax root and no diagnostics.
- `Parse::into_valid_syntax` is the checked boundary for lowering or
  execution; it never publishes a recovery root as valid syntax.
- Imports and includes retain parsed string templates, including
  interpolation expressions, rather than reducing paths to opaque token
  spans.

## Accessors

`expr.@name`, `expr.@["name"]`, and `expr.@(name_expr)` are node/value
accessors. `expr.&name`, `expr.&["name"]`, and `expr.&(name_expr)` are
markup-attribute accessors. Their introducers are contiguous, their
source spans are preserved, and an ordinary optional `?` suffix may
follow them.

Both families compose with every ordinary postfix and assignment
operator. They are never rewritten as object keys. A bare `@name` remains
a format filter.

Static selectors keep their exact selector shape and do not imply blanket
dynamic source authority. `.@attrs` is the complete recovered semantic
attribute-map projection; `.&` selects one expanded-name attribute.

## Inventory

`SyntaxNodeKind::ALL` and the typed `SyntaxNodeRef` / `SyntaxWalk` APIs
are the closed authored-form inventory. Adding an accepted AST form
requires updating the stable inventory, allocation-free child iteration,
and direct-source-span iteration.

`OperatorPrecedence`, `Associativity`, and `InfixOperation` are closed
like the node inventory: wildcard-free matches and the precedence total
order depend on the fixed vocabulary. `BinaryOp`, `AssignmentOp`, and
`TokenKind` are `#[non_exhaustive]` so a new variant is not a downstream
compile break; the authored inventory is still closed
(`OperatorSpec::ALL`, `TokenKind::ALL`, [SYNTAX.md](SYNTAX.md)).
