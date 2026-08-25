# jqf-syntax-bench

Release-mode harness for representative `jqf-syntax` lexing, parsing, typed
traversal, and bound string decoding. Not a conformance matrix; that lives in
`jqf-syntax/tests`.

What it has:

- `lexer/feature-rich-query` — tokenization on realistic syntax
- `parser/short-path` — common small-query fixed cost
- `parser/feature-rich-query` — definitions, bindings, collections, interpolation, `.@` / `.&`
- `parser/string-heavy-query` — escaped text, URLs, JSON-shaped strings
- `parser/interpolation-heavy-query` — repeated source-bound interpolations
- `parser/mixed-postfix-query` — catalog projection with field, index, `.@`, `.&`, optionals
- `parser/large-program` — generated unit with imports and many definitions
- `parser/generated-program-1m` — exactly 1 MiB generated program
- `visitor/generated-program-1m` — typed iterative `SyntaxWalk` on that program
- `string-decode/escaped-256k` — decode 256 KiB of mixed jq escapes

## Invocation

```sh
cargo run --release --locked -p jqf-syntax-bench
cargo run --release --locked -p jqf-syntax-bench -- --quick
cargo run --release --locked -p jqf-syntax-bench -- --json
cargo run --release --locked -p jqf-syntax-bench -- --filter feature-rich
cargo run --release --locked -p jqf-syntax-bench \
  --features allocation-stats -- --allocations
```

Allocation instrumentation is feature-gated through `jqf-bench-core`. The
worker refuses debug builds. Results are machine- and load-specific.

Every lane runs an untimed correctness preflight. Parser lanes retain a
syntax-root receipt with zero diagnostics; the lexer retains an exact token
checksum and one EOF; traversal retains balanced walk counts; string decoding
is compared byte-for-byte with an independently generated expected value.
