# jqf-sdk-ffi Contracts

Invariants for this crate and for hosts. Type overview and examples live
in [README.md](README.md).

This crate does not parse a format grammar, open a file, or evaluate a
program itself. It is the C ABI over `jqf-sdk`, `jqf-engine`, and
`jqf-runtime`.

The checked-in header `include/jqf_sdk_ffi.h` must match the Rust
signatures: same entry points, same arity. `make ffi-header-lint` is the
gate.

## ABI version

- `JQF_ABI_VERSION` is 2. Bindings call `jqf_abi_version` at load and
  refuse a mismatch.
- A bump is any change to an entry point's signature, meaning, or the
  diagnostic record shape.

## Handle

- `jqf_new` / `jqf_new_limited` write `NULL` into `*out` before any
  failure return. A null `out` is `-1`.
- The handle is one thread at a time. Bindings create one per consumer.
- Control and chunk callbacks run on the request thread and must not
  call any `jqf_*` on this handle. A nested call is `-1`, never a hang.
- A poisoned handle (a panic on the request thread) is `-1` with a
  `MACHINE_SETUP` record on a freshly cleared stream.
- `jqf_free` joins the request thread and drops every live program
  while the account is still alive. A null handle is a no-op.
- `limits.control_context` must stay valid for the handle's lifetime.

## Programs

- Program bytes carry an explicit length. An embedded NUL is a typed
  setup error, never a silent truncate.
- `jqf_compile` returns a handle-local `u32`. Ids grow monotonically and
  are never reused. A compile/free loop reclaims the slot immediately.
- A stale id, an id from another handle, or an id past the table is `-1`.
- A NULL binding name or NULL binding arrays is `-1` with `MACHINE_SETUP`
  after a stream clear.
- Programs cannot outlive their handle.

## Publish

- Byte-producing entry points return the required count as `int64_t`, or
  `-1` on failure. The count is 64-bit so a stream larger than 2 GiB
  stays positive.
- `required <= cap` means the buffer holds the complete output.
  `required > cap` means only the first `cap` bytes were written.
- `(NULL, 0)` input is empty input. `(NULL, 0)` output is the sizing
  probe. Both are defined.
- A run publishes into the caller buffer as it goes. On failure, bytes
  already written stay there. `-1` decides failure, never the buffer.
- A `try`/`catch` that absorbs an error is success, including `0` for a
  program that publishes nothing.
- Streaming chunks are flow units, not value frames. A value may
  straddle callbacks.

## Diagnostics

- `jqf_diag_count` / `jqf_diag_get` read the last operation on this
  handle only. Each run or compile clears the stream first.
- `jqf_diag_dropped` is the overflow count on that stream.
- `jqf_diag_get` C-string out-params drop interior NULs (NULL pointer).
  `jqf_diag_get_text` and `jqf_run_error_get` are length-carrying and
  keep interior NULs.
- Text out-params are released with `jqf_diag_free_text`.

## Feed

- `jqf_feed_open` starts a push session on a compiled program. Push,
  poll, finish, close. A closed or unknown feed id is `-1`.
- `jqf_feed_finish` marks end of stream so the held tail is delivered.
  Subsequent polls drain to 0, never `-1`, on a live finished feed.
- A poll that only re-delivers a pending batch, or that reports a death,
  does not clear the diagnostic stream.

## Errors

- Setup failure and uncaught runtime failure are both `-1`.
- User-reachable messages are prose, never Rust syntax.
