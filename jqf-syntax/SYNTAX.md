# jqf language revision 1

This document is the canonical source-syntax contract for `jqf-syntax`.

jqf revision 1 is a jq-shaped language. The catalogued grammar baseline is
jq 1.8.2. Later jq is not implied, and syntax agreement is not runtime
agreement.

## Crate boundary

`jqf-syntax` accepts UTF-8 source text and produces source-preserving tokens,
syntax trees, patterns, source units, and structured diagnostics.

It does not:

- resolve names or decide whether a call is builtin, user-defined, imported,
  or undefined;
- load modules or perform IO;
- assign runtime meaning to operators;
- inspect values, documents, tags, facts, markup nodes, or codecs;
- execute assignments, filters, loops, or calls.

Every name is ordinary syntax. A bare identifier is a zero-argument call.
Parenthesized arguments are filter expressions separated by semicolons.

## Source units

A program source unit contains:

1. an optional leading `module metadata;`;
2. zero or more `import` or `include` declarations;
3. zero or more function definitions;
4. one final query expression.

A library source unit has the same declaration order but may omit the final
query.

```jq
module {name: "example"};
import "math" as math {search: "."};
import "data.json" as $data;
include "strings";
def twice(f): f | f;
math::sqrt(.value)
```

Module and import metadata are parsed as expressions. Constant-object
validation and module loading belong to later crates.

Definitions may also appear at query positions and are lexically scoped over
the query following their terminating semicolon:

```jq
1 | def increment: . + 1; increment
[def normalize: tostring; normalize]
```

## Lexical grammar

### Trivia

ASCII whitespace is trivia. `#` starts a line comment outside a string.
An odd trailing run of backslashes continues a comment onto the next
physical line. Trivia is skipped and is not a token.

### Names

Identifiers use portable ASCII spelling:

```text
[A-Za-z_][A-Za-z0-9_]*
```

Qualified names use one or more `::` separators:

```jq
module::name
a::b::c
```

Variables begin with `$` and may also be qualified:

```jq
$name
$_name
$module::name
$__loc__
```

Bare `$`, `$1`, incomplete qualifications, and the private `$$$$name`
spelling are rejected.

`$__loc__` is a valid variable in expression position and as an
object-constructor shorthand (`{$__loc__}`). It cannot appear as a binder: a
binding pattern, `label` or `break` name, definition parameter, or import
alias.

### Numbers

A number token has no leading sign. `-` is a separate unary operator.

Accepted number spellings include:

```jq
0
01
.5
1.
1.25
1e2
1.e2
.5e-2
```

An exponent marker must be followed by digits after its optional sign.
Number scanning must stop before contiguous `.@` and `.&`, so `1.@tag` and
`1.&unit` preserve the jqf postfix operators.

### Strings and interpolation

Double-quoted strings accept JSON escapes plus interpolation:

```jq
"plain"
"name=\(.name)"
"value=\(def normalize: tostring; normalize)"
```

Interpolation expressions are parsed into ordinary expression trees. Their
spans are absolute byte ranges in the containing source. Invalid escapes,
invalid UTF-16 surrogate sequences, malformed interpolation expressions, and
unterminated strings produce syntax diagnostics.

Format filters remain ordinary syntax:

```jq
@json
@uri
@json "value=\(.)"
```

Supported format names and escaping behavior are runtime concerns.

### Keywords

Reserved *unqualified* syntax words are:

```text
as def module import include if then elif else end
try catch reduce foreach label break and or let
```

`let` is jqf syntax and contextual like `empty`/`true`: `def let:` is a legal
definition, parameter, and import-alias name, and after `def let: …` a bare
`let` is that definition.
The binder form `let PAT = SRC | BODY` keeps its role; `let(args)` and
`let::name` are calls.

`empty`, `null`, `true`, and `false` keep their primary syntax roles and are
legal definition, parameter, and import-alias names. After `def empty: …`, a
bare `empty` is that definition. After `def true: …` / `def false: …` /
`def null: …`, the bare spelling is still the literal; `true(args)` and
`true::name` (and the `false`/`null` twins) are calls.

Keywords and literal-like words remain valid static property names in the
positions that accept them:

```jq
.if
.null
{then: .value}
{empty}
```

`not` is an ordinary zero-argument call, not a syntax keyword.

## Fixed token inventory

The symbolic inventory is:

```text
. .. .@ .& ? ?// | , : ; :: ( ) [ ] { } ~
+ - * / % == != < <= > >= //
= |= += -= *= /= %= //= =>
```

The lexer always chooses the longest valid token.
It also recognizes reserved `=>` as one targeted error token; `=>` is not an
accepted jqf expression operator.

## Engine surface

The `~` marker introduces the engine surface, which is source syntax for
engine constructors and engine bindings:

```jq
~name            # a bare engine term: an engine-binding reference, or an
                 # engine-constructor name about to be called
~name(args)      # an engine-constructor call, e.g. ~generator(0; .+1; .)
as ~x | body     # an engine-binding pattern, lexically scoped like $x
```

`~name(args)` parses to an engine-constructor call and `as ~x` to an
engine-binding pattern; lowering resolves the name against the engine scope
and the closed constructor list. An unmarked `name` stays an ordinary call.
The expression span includes the `~` introducer; the name span does not.

## Precedence and associativity

Higher rows bind tighter:

| Level | Forms |
| --- | --- |
| Primary/postfix | literals, calls, groups, collections, fields, indexes, slices, iterators, `.@`, `.&`, `?` |
| Prefix/control | unary `-`, `if`, `try`, `reduce`, `foreach`, `label`, `break`, `let`, format filters |
| Multiplicative | `*`, `/`, `%` |
| Additive | `+`, `-` |
| Comparison | `==`, `!=`, `<`, `<=`, `>`, `>=` |
| Logical and | `and` |
| Logical or | `or` |
| Assignment | `=`, `|=`, `+=`, `-=`, `*=`, `/=`, `%=`, `//=` |
| Alternative | `//` |
| Binding | `source as pattern | body` |
| Comma | `,` |
| Pipe | `|` |

Multiplicative, additive, logical, and comma forms are left-associative.
Alternative and pipe are right-associative.
Comparison and assignment accept exactly one unparenthesized operator; further
comparison or assignment requires an explicit group.

Compatibility points:

- assignment binds tighter than alternative and comma;
- comma binds tighter than pipe;
- call arguments are separated by `;`, so a comma inside one argument remains
  generator syntax;
- object-member commas are separators, not generator nodes;
- a complete group preserves authored grouping rather than widening its child.

## Primary expressions

```jq
.
..
empty
null
true
false
42
"text"
$variable
name
module::name
(expression)
```

Bare and qualified names are zero-argument calls. Empty parenthesized
calls such as `name()` are syntax errors; use `name`.

`empty`, `null`, `true`, and `false` followed by `(` or `::` parse as ordinary
calls (`empty(5)`, `true::foo`). A bare `empty` stays the empty syntax form; a
bare `true` / `false` / `null` stays the literal.

## Postfix paths

Postfixes compose on primary and control expressions:

```jq
expr.name
expr."quoted-key"
expr[index]
expr.[index]
expr[start:end]
expr.[start:end]
expr[]
expr.[]
expr?
```

Index and slice expressions are full expressions. Either slice bound may be
omitted. A path postfix may carry the ordinary optional `?` suffix.

Common identity-rooted shorthands are:

```jq
.name
."quoted-key"
.[index]
.[:end]
.[]
```

`..` is a distinct recursive-descent primary, not syntax sugar for a call. It
accepts ordinary postfixes:

```jq
..[0]
...name
...@tag
...&href
```

Exact adjacency matters: `..name`, `.a..`, and `1..2` are invalid. A
trailing-dot number may still be followed by a path:
`1..field` is the number `1.` followed by `.field`.

## jqf node/value and attribute accessors

`.@` and `.&` are required jqf postfix operators.

Node/value access:

```jq
expr.@name
expr.@["quoted-name"]
expr.@(name_expression)
```

Markup attribute access:

```jq
expr.&name
expr.&["quoted-name"]
expr.&(name_expression)
```

The introducer is contiguous: whitespace between `.` and `@` or `&` is a
targeted syntax error. Bare `@name` remains a format filter.

Every accessor node preserves:

- the complete operator span;
- direct, bracketed, or dynamic selector shape;
- selector and delimiter spans;
- the optional-suffix span when present.

Both accessor families compose with all ordinary postfixes and assignments:

```jq
.price.@tag
.name.@comment = ["display name"]
.name.@comment |= . + ["more"]
.a.&href
.a.&["aria-label"]?
.a.&href = "/docs"
```

`.@attrs` is the complete recovered semantic attribute-map projection. `.&`
selects one expanded-name markup attribute. Which accessor names and
capabilities exist is not a parser decision.

`Value::Tagged` remains the single authoritative owned representation for a
non-core tag. `.@tag` exposes the intrinsic tag through syntax; it does not
authorize a second independently mutable fact store.

## Collections

Arrays contain one optional generator body:

```jq
[]
[expression]
[1, 2, 3]
```

Comma inside an array remains executable generator choice. It is not an AST
vector of array elements.

Objects contain ordered members:

```jq
{}
{name: .name}
{"quoted": .value}
{$variable: .value}
{(key_expression): .value}
{name}
{"quoted"}
{$variable}
{name: .value,}
```

An unparenthesized dynamic key such as `{.key: .value}` is invalid. A dynamic
key without a value such as `{(.key)}` is also invalid. Object-member commas
are contextual separators; a generator value must be grouped:

```jq
{values: (1, 2)}
```

## Operators and assignment

Unary:

```jq
-expression
```

Binary:

```jq
lhs * rhs
lhs / rhs
lhs % rhs
lhs + rhs
lhs - rhs
lhs == rhs
lhs != rhs
lhs < rhs
lhs <= rhs
lhs > rhs
lhs >= rhs
lhs and rhs
lhs or rhs
lhs // rhs
lhs, rhs
lhs | rhs
```

Assignments are distinct typed syntax nodes:

```jq
target = value
target |= update
target += value
target -= value
target *= value
target /= value
target %= value
target //= value
```

Syntax preserves target, value, operator identity, and exact operator span.
Target validity and update behavior belong to the engine. This includes
ordinary paths, `.@` paths, and `.&` paths.

## Bindings and patterns

Binding:

```jq
source as pattern | body
```

jqf binding sugar:

```jq
let pattern = source | body
```

`let pattern = source | body` binds the whole source expression; `source as
pattern | body` binds each source output. `1, 2 as $x | $x` emits only `2`;
`let $x = 1, 2 | $x` emits `1` then `2`. The `=` in `let` is a separator, not
an assignment node.

Patterns:

```jq
$name
[$first, $second]
{$name, key: $value}
{("dynamic"): $value}
pattern ?// pattern
```

Empty array/object patterns, trailing pattern commas, shorthand non-variable
object-pattern keys, and nested `?//` inside an array or object pattern are
rejected.

## Control forms

Conditionals preserve every source-ordered branch and whether `else` was
authored:

```jq
if condition then consequent end
if condition then consequent else alternative end
if a then b elif c then d else e end
```

Error handling:

```jq
try expression
try expression catch handler
expression?
```

The `try` operand stops before a following infix operator. A compound protected
expression requires grouping. Without `catch`, following infix operators remain
outside the protected term:

```jq
try (1 + 2) catch .
try 1 + error("not protected")
```

Folds require an explicit binding:

```jq
reduce source as pattern (initial; update)
foreach source as pattern (initial; update)
foreach source as pattern (initial; update; extract)
```

`reduce` never has an extract slot. Omitted fold bindings are invalid.

Labels:

```jq
label $name | body
break $name
```

Control forms with an explicit terminator remain postfix-composable:

```jq
if .ok then {} else {} end.name
reduce .[] as $item ({}; .).name
```

## Calls and definitions

Calls:

```jq
name
qualified::name
name(argument)
name(first; second)
name((1, 2))
```

Each argument is a full filter expression. `name(1, 2)` is one generator
argument, not two arguments.

Definitions:

```jq
def name: body;
def name($value; filter): body;
```

Definition parameters are separated by semicolons and may not be empty.
Multiple definitions with the same name and arity are syntax-valid; resolution
is later.

## Source preservation and diagnostics

All public spans are half-open UTF-8 byte ranges in one source. Syntax trees
preserve:

- exact literal, name, variable, keyword, delimiter, and operator coverage;
- grouping;
- shorthand versus explicit object members;
- source-item keywords, metadata, parameters, separators, and terminators;
- call and definition argument separators and authored parentheses;
- control-form keywords, loop separators, binding form, and binding pipe;
- string interpolation introducers, expressions, and closing delimiters;
- postfix operator, selector, bracket/parenthesis, and optional-suffix spans;
- source-item order.

The parser emits stable structured diagnostics with source labels. Recoverable
parsing may publish error nodes. `Parse::is_valid` and
`Parse::into_valid_syntax` provide the checked boundary callers must use before
lowering or execution.

`TokenKind::ALL` is the complete token inventory for the language, and
`SyntaxNodeKind::ALL` is the closed authored-form inventory. Both the node
accessor (`.@`) and the attribute accessor (`.&`) are mandatory; omitting
either is a language bug.
