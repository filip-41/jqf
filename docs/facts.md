# Facts: `.@` and `.&`

Used when a node carries things that are not its value: a YAML tag, a TOML comment, or an
HTML attribute. jqf calls them **facts** and make them an ordered, portable metadata on a
document node: `.@name` reads one fact, `.&name` reads one markup attribute. The
same spellings write, and [`--edit`](editing.md) splices what they wrote into
the source. `jqf --help facts` is the live surface.

```console
$ echo '!money 5' | jqf --input-format yaml '.@tag'
"!money"

$ printf 'port = 8080 # main port\n' | jqf --input-format toml -c '.port.@comment_inline'
["main port"]

$ echo '<a href="https://x">y</a>' | jqf --input-format xml '.&href'
"https://x"
```

## The catalogue

| Fact                                 | Payload                                              | Carried by                                        |
| ------------------------------------ | ---------------------------------------------------- | ------------------------------------------------- |
| `.@comment` (alias `.@comment_head`) | list of lines above the node                         | TOML, YAML, JSONC, JSON5, properties, INI, dotenv |
| `.@comment_inline`                   | lines on the value's own line                        | the same family                                   |
| `.@comment_foot`                     | lines below / the document trailer                   | the same family                                   |
| `.@tag`                              | the tag text (`"!money"`, `cbor:tag:<n>`)            | YAML, CBOR, MessagePack ext, jqft                 |
| `.@style`                            | `plain` / `single` / `double` / `literal` / `folded` | YAML                                              |
| `.@anchor`, `.@alias`                | anchor / alias name                                  | YAML                                              |
| `.@name`                             | the element name (Clark form under namespaces)       | XML, HTML, jqft markup                            |
| `.@attrs`                            | the attribute map                                    | XML, HTML                                         |
| `.@content`                          | the joined text content                              | XML, HTML                                         |
| `.&attr`                             | one attribute's value                                | XML, HTML                                         |

Bracket and dynamic spellings work, so formas like
`.a.&["aria-label"]` and `.@("com" + "ment")` can be used. The vocabulary is flat — a path
*through* a fact (`.name.@comment.leading`) is a compile error.

> The flat vocabulary law is enforced purely due to lack of nested consumers in supported
> formats. The idea can be reconsidered if there's ever a need for it

## Reads are total

A missing fact reads `null`. Reads other than `.@tag` need a
located document node.

> **facts are provenance, not data**. Any operation that
> constructs a new value drops them.

```console
$ printf '# main port\nport = 8080\n' | jqf --input-format toml -c '(.port + 0) | .@comment'
null
```

To keep a comment across a value change, do it under [`--edit`](editing.md): a
leaf patch never touches the comment bytes, so nothing needs preserving. The
explicit round-trip (read the fact before the construction, write it back
after) spells the same thing, and `|=` on the fact preserves and extends in
one step:

```console
$ printf '# main port\nport = 8080\n' | jqf --edit --input-format toml '.port.@comment as $c | .port += 1 | .port.@comment = $c'
# main port
port = 8081

$ printf '# main port\nport = 8080\n' | jqf --edit --input-format toml '.port += 1 | .port.@comment |= . + ["bumped by ops"]'
# main port
# bumped by ops
port = 8081
```

On a plain run the drop is final and a re-encoded document
loses authored comments, and a comment written after a construction has no
located node to land on. A structural edit (replacing the leaf with a new
container) re-renders the node and refuses to mix in a fact assignment.

## Writes

A fact assignment compiles on any run, but where it can *land* depends on the lane:

- **Under `--edit`** the assignment is a span delta against the retained source
  — comment lines rewritten in place, an attribute's quoted bytes spliced, a
  YAML style/tag/anchor/alias rewritten. No value mutation is involved.
- **On a plain run** only comment facts encode (they attach to the rendered
  document). An attribute or YAML-role write outside `--edit` is refused — no
  output path would apply it.
- A format that cannot carry the fact refuses: a strict-JSON `--edit` comment
  write is a usage error.

```console
$ echo '<a href="https://x">y</a>' | jqf --edit --input-format xml '.&href = "/docs"'
<a href="/docs">y</a>

$ printf 'foo: bar\n' | jqf --edit --input-format yaml '.foo.@style = "double"'
foo: "bar"
```

The writable node roles are a closed list: `comment`, `comment_inline`,
`comment_foot`, `style`, `tag`, `anchor`, `alias`, and any attribute name
through `.&`. An unknown role raises.

## `--json-facts`

Markup would be unreadable as bare values. The value of an element is just its
children, so every name and attribute lives in facts. `--json-facts` projects
facts into the output as an xq-style tree, and it is **on by default** for
XML/HTML input with JSON output:

```console
$ echo '<a href="https://x">y</a>' | jqf --input-format xml -c .
{"a":{"@href":"https://x","#text":"y"}}

$ echo '<a href="https://x">y</a>' | jqf --input-format xml --no-json-facts -c .
["y"]
```

Element names become keys, attributes `@attr` keys, joined text `#text`,
repeated siblings arrays. The projection is lossy by design, so a data key wins a
collision with a fact key. Root-level paths differ between the two dials. 
The same projection is callable as the `json_facts` builtin.
