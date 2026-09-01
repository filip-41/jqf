# HTML and XML

The HTML codec is a WHATWG-conformant parser, so real-world tag soup recovers
exactly the way a browser recovers it. The XML codec is a secure,
namespace-aware XML 1.0 parser. Both project elements as values with names and
attributes as [facts](facts.md).

## The HTML parser

The decoder implements the WHATWG HTML Standard, with the §13.2.5 tokenizer and
the §13.2.6 tree-construction algorithm: insertion modes, foster parenting, the
adoption agency, active formatting elements, template contents. Decoding is also
held against the html5lib conformance suites. Scripting is disabled: scripts
never execute, and the codec never fetches a URL or touches the network.

Encoding is determined the WHATWG way: a UTF-8 BOM, else a `meta charset` scan
of the first 1024 bytes, else windows-1252. UTF-16 content refuses.

Two input dialects:

- `html.document@1` — a full document, with the parser's whole recovery.
- `html.fragment@1` — the WHATWG fragment parsing algorithm, in a fixed `div`
  context: plain text, partial markup, an element soup are all accepted as
  fragments.

```console
$ printf '<em>x</em>' | jqf --input-format html --input-dialect html.fragment@1 -c .
{"html":{"em":"x"}}
```

HTML decodes and encodes (`html.source@1` echoes an unchanged document,
`html.document-serialize@1` re-serializes). It refuses `--edit` — recovery
rewrites the tree, so there is no authored span to splice.

## The projection

An element is an array of its children; the element's name, attributes, and text
are facts. Because the bare value would drop every element name, markup input to
JSON output turns `--json-facts` **on by default**, producing the xq-style tree:
element names as keys, attributes as `@attr`, text as `#text`, repeated siblings
as arrays.

```console
$ printf '<html><body><div id="a" class="x"><p>hi</p></div></body></html>' | jqf --input-format html -c .
{"html":{"head":null,"body":{"div":{"@id":"a","@class":"x","p":"hi"}}}}

$ echo '<a href="https://x">y</a>' | jqf --input-format xml --no-json-facts -c .
["y"]
```

The projection is lossy by design (data keys win over fact keys). The fact
accessors are the lossless surface: `.&href` reads one attribute, `.@name` the
element name, `.@attrs` the attribute map. Root-level paths differ between the
two dials.

## Scraping

The [`css` builtin](selectors.md) evaluates selectors against the recovered
HTML document and streams matching elements in document order. Combined with
the fact accessors, that is the scraping surface:

```console
$ printf '<html><body><ul><li><a href="/a">A</a></li><li><a href="/b">B</a></li></ul></body></html>' \
    | jqf --input-format html -c '[css("li a") | .&href]'
["/a","/b"]
```

jqf reads bytes you hand it, so it needs to be paired with another tool like
`curl`:

```bash
curl -s https://example.com | jqf --input-format html -c '[css("h1")]'
```

## XML

Input `xml.document@1`: non-validating XML 1.0 with the namespace stack: no
external entities, no DTD fetching. Element names are expanded `(URI, local)`
pairs, spelled Clark-style `{uri}local` where a name is answered, namespace
declarations are not ordinary attributes. The [`xpath` builtin](selectors.md)
evaluates a closed XPath subset against this document; it is XML-only
(`css` is HTML-only).

```console
$ printf '<r a="1"><c>x</c><c>y</c></r>' | jqf --input-format xml -c .
{"r":{"@a":"1","c":["x","y"]}}
```

Output is `xml.source@1` (echo unchanged) or `xml.jqf-deterministic@1`, which
re-binds namespace URIs to deterministic prefixes. Unlike HTML, XML declares
Edit — attribute and text spans splice in place:

```console
$ echo '<a href="https://x">y</a>' | jqf --edit --input-format xml '.&href = "/docs"'
<a href="/docs">y</a>
```
