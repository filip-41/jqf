# jqf-codec-render

Output-only presentation renderers: eight base profiles — plain text, GFM
markdown table, HTML table fragment, ASCII grid table, tree, terminal-safe
styled text, POSIX `sh` assignments, and a plain-ASCII frequency histogram —
each composed with the layout/width/header option law.

This crate is `no_std` and uses `alloc`. It depends on `jqf-codec-core` for
the route contracts, `jqf-source` for spans, `jqf-resource` for the work
budget, and `jqf-data` for documents and values.

What it has:

- `registration()` — the ENCODE-only registration; no input provider,
  detection, or round-trip claim
- `FORMAT_ID` and the eight renderer dialect ids (`PLAIN_DIALECT_ID`,
  `GFM_TABLE_DIALECT_ID`, `HTML_TABLE_DIALECT_ID`, `GRID_TABLE_DIALECT_ID`,
  `TREE_DIALECT_ID`, `TERMINAL_DIALECT_ID`, `SHELL_DIALECT_ID`,
  `HIST_DIALECT_ID`)
- `RenderEncodeOptions` with the header policy, width profile, and terminal
  shape composition knobs
- the RECORD route (one frame per record) plus ADJACENT VALUES (one frame per
  item in a multi-item run)

It does not evaluate programs or own I/O.

```rust
use jqf_codec_render::{FORMAT_ID, registration};

assert_eq!(FORMAT_ID, "render");
assert!(registration().is_ok());
```

Family laws: [`CONTRACTS.md`](../CONTRACTS.md).
