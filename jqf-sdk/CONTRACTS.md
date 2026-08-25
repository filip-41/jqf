# jqf-sdk Contracts

Invariants for this crate and for hosts. Type overview and examples live
in [README.md](README.md).

This crate does not parse a format grammar, evaluate a program, open a
file or a socket, or decide facade framing policy. It selects codec
registrations, preserves engine item boundaries, and accounts published
bytes.

An embedder compiles against this crate plus one codec crate. Types
owned by `jqf-engine`, `jqf-resource`, `jqf-source`, `jqf-data`, and
`jqf-codec-core` are re-exported here without wrappers.

## One entry

- `execute` is the only public routing entry. The named drives are
  crate-private. A second public `execute` is a contract break
  (`tests/public_surface.rs`).
- A request that cannot be honoured is refused. It is never silently
  degraded.

## Request

- `Input::Whole` is one retained buffer. `Input::Streaming` is a pull
  callback the SDK never opens; `Ok(0)` is EOF. `Input::Records` is
  physically framed records the embedder opened.
- `Request` and `ResourceContext` are `!Send`. Spawn a sized thread
  (`request_stack_bytes`) first; construct the request on that thread;
  join only the `Result`.
- Compile and execute recurse on the call stack. The documented
  `10_000` nesting refusal needs the default 256 MiB request stack.

## Routing and publication

- The SDK picks the drive from the request. It does not expose the
  drive names.
- Engine item boundaries are preserved. One engine item is one sink
  item.
- Output publication is accounted on the request ledger before the
  bytes leave. A refused output charge is a pipeline failure, not a
  truncated item.
- `-n` and `-s` cannot combine with the streaming event drive. The
  request constructor refuses that pair.

## Catalog

- A request names the registrations it may select. A missing format or
  dialect is a pipeline failure, not a fallback to another registration.
- Default-on extension families follow `jqf-engine`.
  `--no-default-features` drops every family.

## Errors

- Pipeline failures keep a closed cause: codec, registry, sink, or
  raised program error. They render as words, not as boxed `std`
  errors.
- `Outcome::Declined` is a drive that will not honour the request.
  It is not a failure and publishes nothing.
