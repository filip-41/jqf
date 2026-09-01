# Selectors: CSS, XPath, JSONPath

Three query languages are implemented as builtins: `css` against HTML
documents, `xpath` against XML documents, and `jsonpath` against any
value. All three are ordinary builtins and their results pipe into the
rest of the program. See [HTML and XML](html.md).

## `css(SELECTOR)`

Selectors Level 4, the static profile: type / universal / id / class / attribute
selectors, all four combinators, `:is` / `:where` / `:not` / `:has`, the
structural `:nth-*` family (including `of`), `:root`, `:empty`, `:scope`,
`:lang`, `:dir`. Dynamic pseudo-classes (`:hover`) and pseudo-elements
(`::before`) are rejected at compile.

Results are elements in document order (deduplicated). It serves HTML documents.
In quirks mode id and class match ASCII case-insensitively, in standards mode
case-sensitively.

```console
$ printf '<html><body><ul><li><a href="/a">A</a></li><li><a href="/b">B</a></li></ul></body></html>' \
    | jqf --input-format html -c '[css("li a") | .&href]'
["/a","/b"]
```

## `xpath(EXPR)`

A closed XPath subset over XML documents: absolute and relative paths,
`//`, `.` and `..`, the element axes (child, descendant, descendant-or-self,
parent, self), wildcards, unions, and predicates — positions, `position()` /
`last()`, comparisons, `count()`, `concat()`, `string-length()`, `name()`.

Results are elements only: `@attr` and `text()` as *result* axes are compile
errors (read attributes with [`.&`](facts.md) and text through the value).
Namespaced names are spelled `Q{uri}local` and there is no prefix environment, so
`prefix:local` is rejected.

```console
$ printf '<catalog><item id="1"/><item id="2"/></catalog>' | jqf --input-format xml -c '[xpath("//item") | .&id]'
["1","2"]

$ printf '<catalog><item/><item/></catalog>' | jqf --input-format xml -c 'xpath("count(//item)")'
2
```

Both selector builtins are budgeted (selector length, candidate walks, result
counts) and both raise a catchable error on a non-markup input:

```console
$ echo '{}' | jqf -c 'try css("div") catch .'
"css serves html documents; the input is a json document"
```

## `jsonpath(QUERY)`

RFC 9535, the full surface: root `$`, wildcards, slices with steps, unions,
descendant segments, filter expressions with `@`, and the standard functions
(`length`, `count`, `match`, `search`, `value`). Regular expressions are
I-Regexp via the engine's regex.

Each query answers **one array** (the nodelist of matched values):

```console
$ echo '{"store":{"book":[{"title":"A","price":5},{"title":"B","price":15}]}}' | jqf -c 'jsonpath("$.store.book[*].title")'
["A","B"]

$ echo '{"store":{"book":[{"title":"A","price":5},{"title":"B","price":15}]}}' | jqf -c 'jsonpath("$.store.book[?@.price < 10].title")'
["A"]
```

A missing path is `[]` and an invalid query raises (catchable). A
bare query without `$` is accepted as shorthand, `jsonpath(".a")` is
`jsonpath("$.a")`. The two-argument form `jsonpath(SOURCE; QUERY)` runs each
query over each source.

The family is feature-gated as `ext-jsonpath` in
[embedded builds](embedding.md).
