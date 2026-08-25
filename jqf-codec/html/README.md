# jqf-codec-html

Defines WHATWG-recovered HTML documents and context-bound fragments.

This crate is `no_std` and uses `alloc`. It depends on `jqf-codec-core`,
`jqf-data`, `jqf-resource`, and `jqf-source`. It owns the HTML grammar.

What it has:

- `registration()` / `registration_fragment()` — catalog entries for the
  document dialect plus two output profiles, and for the fragment dialect
- `html.document@1` / `html.fragment@1` — input dialects
- `html.source@1` / `html.document-serialize@1` — output profiles
- whole-document decode over the tokenizer and tree builder, plus an
  exact-path located route
- `FORMAT_ID`, the dialect ids, and `FRAGMENT_DEFAULT_CONTEXT`
- `tokenizer_core` / `tree_core` — the conformance-harness surface

It does not execute scripts, fetch URLs, own I/O, or splice `--edit`.

```rust
use jqf_codec_html::{FORMAT_ID, HTML_DOCUMENT_DIALECT_ID, registration};

let registration = registration().unwrap();
assert_eq!(registration.descriptor().format().as_str(), FORMAT_ID);
assert!(registration
    .descriptor()
    .dialects()
    .iter()
    .any(|d| d.as_str() == HTML_DOCUMENT_DIALECT_ID));
```

## Document and fragment

`registration()` serves `html` (extensions `html`, `htm`). The document
dialect recovers a full document. The two output profiles live on the
same registration.

```rust
use jqf_codec_html::{HTML_DOCUMENT_SERIALIZE_DIALECT_ID, HTML_SOURCE_DIALECT_ID, registration};

let dialects: Vec<_> = registration()
    .unwrap()
    .descriptor()
    .dialects()
    .iter()
    .map(|d| d.as_str())
    .collect();
assert!(dialects.contains(&HTML_SOURCE_DIALECT_ID));
assert!(dialects.contains(&HTML_DOCUMENT_SERIALIZE_DIALECT_ID));
```

`registration_fragment()` serves `html.fragment@1` with no extensions.
The fragment algorithm uses one fixed context element, `div`.

```rust
use jqf_codec_html::{FRAGMENT_DEFAULT_CONTEXT, HTML_FRAGMENT_DIALECT_ID, registration_fragment};

assert_eq!(FRAGMENT_DEFAULT_CONTEXT, "div");
let registration = registration_fragment().unwrap();
assert!(registration
    .descriptor()
    .dialects()
    .iter()
    .any(|d| d.as_str() == HTML_FRAGMENT_DIALECT_ID));
```

## Decode

The front end picks an encoding (BOM, then a 1024-byte `meta charset`
prescan, then windows-1252), tokenizes the decoded text, and builds a
tree. Scripting is off. A document is one document: HTML is not an
adjacent-value format.

An element is an array of its recovered children. Comments attach as
facts, not child values.

## Encode

`html.source@1` echoes the sealed source bytes of an unchanged whole
document. `html.document-serialize@1` writes one UTF-8 BOM and then the
document element. A doctype-bearing document has no serialize spelling.

## Contracts

See [`CONTRACTS.md`](CONTRACTS.md) for recovery, projection, and encode
invariants. Family laws: [`../CONTRACTS.md`](../CONTRACTS.md).
