# jqf-codec-ini Contracts

Invariants for this crate and for hosts. Type overview and examples live
in [README.md](README.md).

This crate does not evaluate programs, interpolate `$VAR`, open files, or
claim `.conf`. Why three format ids live in [README.md](README.md).
Family bind, publish, and error laws live in
[../CONTRACTS.md](../CONTRACTS.md).

## Membership

Three format ids, one crate. The grammar is sealed per registration, not
an option.

`ini.jqf-strict@1` and `dotenv.jqf-strict@1` are crate-defined
conservative intersections. They are not "INI conformance" or a shell
evaluator.

## Properties (`properties.jdk@1`)

- A logical line ends at `\n`, `\r\n`, `\r`, or end of stream.
- A line of only space, tab, or form feed is ignored.
- `#` or `!` as the first non-whitespace character of a natural line is a
  comment. A trailing `\` on a comment is literal, not a continuation.
- `\` immediately before a line terminator continues the logical line.
- The key runs from the first non-whitespace character to the first
  unescaped `=`, `:`, or whitespace. Those characters enter the key only
  escaped. Whitespace after the key is skipped; a following `=` / `:` is
  consumed with its trailing whitespace.
- The value is the rest of the logical line with escapes cooked: `\uXXXX`
  (with surrogate pairing), `\t`, `\n`, `\r`, `\f`, and any other `\x` as
  a literal `x`. Trailing raw whitespace is value text.
- A malformed `\uXXXX`, a high surrogate not followed by a valid low
  escape, a BOM, and any non-UTF-8 byte are terminal failures.
- A line with no key (`=value`) is an empty key. A key with nothing after
  the separator is an empty value.

## INI (`ini.jqf-strict@1`)

- `[section]` alone on a line. The name is the exact byte run between the
  brackets, trimmed of surrounding spaces. No subsections.
- `key = value` or `key: value`. The key is the text before the first
  separator, trimmed. The value is the rest of the logical line, trimmed.
- `;` or `#` at the first non-blank byte of a line is a comment. An
  inline `;` / `#` after a value is value text.
- No line continuations, no quote processing, no `\` escapes. A leading
  or trailing quote is part of the value.
- A key line before any section belongs to the root object.
- A bare key with no separator, an unterminated `[`, an empty section
  name, a duplicate section header, and a root-key / section-name
  collision are terminal failures.

## dotenv (`dotenv.jqf-strict@1`)

- `#` at the first non-blank byte of a line is a comment.
- The separator is the first `=`. The key is trimmed.
- Single-quoted values are literal. Double-quoted values take `\n \r \t
  \\ \" \$`. Unquoted values are literal.
- `$VAR` is not interpolated.
- A leading `export ` is accepted and stripped. Canonical encode writes
  none.
- A line without `=`, an unterminated quote, and trailing content after a
  quoted value are terminal failures.

## Value model

Every value is a string. No type inference. Keys are literal strings,
never split. Sections nest exactly one level. Duplicate keys follow the
object-builder law: first insertion fixes position, last occurrence
supplies the value. Leading comment runs attach as the dialect's comment
fact on the value node. Comments after the last entry attach as the
root's foot-comment fact.

Member order is canonical, not authored: every section object attaches
to the root before any root-level scalar. Encode emits root scalars
first, then sections.

The scan is terminal (no recovery dialect) and corrupt-late: a bad byte
anywhere fails the whole document. Work is admitted once per logical
line.

## Encode

String, integer, decimal, float, and bool render as canonical scalar
text. Null, arrays, tags, and objects beyond the one INI section level
are unrepresentable. A value containing a line terminator is escaped
under properties, unrepresentable under ini, and double-quoted under
dotenv.

A leading blank cannot sit unquoted in any of the three grammars.
Properties escapes it; ini refuses; dotenv quotes. A trailing blank
round-trips under properties verbatim; ini refuses a value it would
trim; dotenv quotes.

An INI section name that is empty or contains `]` is unrepresentable. A
dotenv key that starts with `export ` is unrepresentable (the decoder
would strip it). A decimal whose scale magnitude cannot fill in 4096
zeros is unrepresentable.

The output profile writes `key=value` lines, LF-terminated. INI writes
`[section]` headers for the one legal nesting level.

## Edit

A leaf patch replaces the value's authored span, never the key and never
the newline. A grown container splices a new statement in the local
syntax and copies the nearest sibling's separator spacing. A removed
entry is its line cut, including the `#` / `;` / `!` comment and blank
lines above it.
A removed INI section is declined: cutting only the header would leave
its members as root keys.

An edit to a duplicated key patches the winning (last) occurrence. The
shadowed earlier occurrences stay untouched bytes.

## Registration

- `properties`: extension `properties`. Dialects `properties.jdk@1` /
  `properties.jqf-1.0@1`.
- `ini`: extensions `ini`, `cfg`. Not `conf`. Dialects `ini.jqf-strict@1`
  / `ini.jqf-1.0@1`.
- `dotenv`: no extension. Filenames `.env` and `.env.*`. Dialects
  `dotenv.jqf-strict@1` / `dotenv.jqf-1.0@1`.

Each registration serves the edit lane. Flat config is one document per
source, not a record stream and not an adjacent-value format. The family
has no tag vocabulary.

## Boundaries

Access slots are Whole/`CompleteDocument` and Exact/`Located`. Exact
scans the whole input, materializes only the hit, and republishes it as
the product root. Adjacent values are a requirement mismatch.
