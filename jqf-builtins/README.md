# jqf-builtins

Owns jqf's builtin vocabulary, semantic evaluators, error vocabulary, and
closed builtin registry.

This crate is `no_std` and uses `alloc`. `jqf-engine` owns compilation,
analysis, and execution, and depends on this crate one-way.

What it has:

- the builtin family and overload catalogs
- dispatch from stable overload ids to evaluators and lowerings
- value semantics for core and extension builtins
- selector implementations for paths, regexes, JSONPath, XPath, and CSS
- builtin error messages and result contracts
- constant-evaluation helpers

The wide Rust visibility is an internal boundary between this crate and
`jqf-engine`. Applications should use the curated `jqf-engine` or `jqf-sdk`
APIs.
