# The vendored HTML corpus

## `entities.json`

The WHATWG named-character-reference table, vendored from the WHATWG HTML
standard's `entities.json`. The digest
`d741d877ac77c4194c4ad526b5b4a19aef8dfe411ab840a466891cdbb9f362e6`
is pinned on the generated `src/entities_table.rs`. This crate's `build.rs`
does not re-hash the file.

## `tokenizer/` and `tree-construction/`

Conformance suites vendored from html5lib-tests (`tokenizer/*.test` and
`tree-construction/*.dat`, renamed `tc-*.dat` here). They are the recovery
oracle for `html.document@1`. This crate's unit tests do not read them; the
receipts harness that walks the suites lives with the origin tree until that
tool ports. Eight noscript `#document` trees were re-baselined from the
scripting-enabled RAWTEXT shape to the scripting-disabled element shape the
codec pins.
