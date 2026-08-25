# jqf-codec-xml

XML 1.0 decode and encode for jqf.

This crate is `no_std` and uses `alloc`. It depends on `jqf-codec-core`,
`jqf-data`, `jqf-resource`, and `jqf-source`. It owns the XML grammar.

What it has:

- `registration()` — one catalog entry carrying the input dialect
  (`xml.document@1`) and two output profiles (source echo,
  `jqf-deterministic`)
- whole-document decode over a secure non-validating XML 1.0 parser with a
  namespace stack: elements, attributes, mixed content, entities,
  declarations, comments, processing instructions, CDATA
- an exact-path located route and a scoped walk over the private element
  tree
- deterministic rewrite encode under the two output-profile ids

It does not validate against a DTD or schema, resolve external entities,
or own I/O.

```rust
use jqf_codec_xml::{FORMAT_ID, XML_DOCUMENT_DIALECT_ID, registration};

let registration = registration().unwrap();
assert_eq!(registration.descriptor().format().as_str(), FORMAT_ID);
assert!(registration
    .descriptor()
    .dialects()
    .iter()
    .any(|d| d.as_str() == XML_DOCUMENT_DIALECT_ID));
```

Family laws: [`CONTRACTS.md`](../CONTRACTS.md).
