# jqf (Python) Contracts

Invariants for this package and for hosts. Type overview and examples
live in [README.md](README.md).

This package does not parse a format grammar, evaluate a program, or
open a file itself. It is the ctypes facade over `jqf-sdk-ffi`.

## ABI version

- `JQF_ABI_VERSION` is 2, matching `jqf-sdk-ffi`. Import refuses a
  mismatch.
- A bump is any change to an entry point's signature, meaning, or the
  diagnostic record shape.

## Library load

- `JQF_FFI_LIB` wins. Then a copy next to the package, then
  `target/release`, then a bare soname on the loader path.
- An installed wheel must not pick up a different library from the
  system. The in-package copy is why that copy is tried first.

## Handle

- `jqf_new` builds an unlimited handle. This binding does not call
  `jqf_new_limited`.
- The handle is one thread at a time. Every ABI call on a `Session`
  takes the session lock. Close marks the handle dead under that
  same lock.
- A call after `close` is `JqfError` with a dead-session record. It
  does not pass a freed pointer into the library.

## Publish

- Byte-producing calls follow the snprintf convention. `written >
  cap` is the exact size of the next call, not a truncated success.
- The growth loop retries at most four times. Exhaustion is
  `JqfTruncatedError`, never a silent prefix.
- The first attempt is sized from the input, capped at 64 MiB, so a
  huge input does not stage sixteen times its size before the first
  byte is known.

## Failure

- A terminal failure is the last uncaught error record (`severity ==
  "E"` and `caught is None`), or a halt record. `written < 0` still
  fails when the stream is empty.
- `try` / `catch` that absorbs an error is success.
- Per-value errors ride `RunResult.errors` beside a successful
  output. They do not become `JqfError`.

## Programs and feeds

- A program or feed is a handle-local `u32`. Ids are never reused.
- A stale id, a foreign id, or a use after `Session.close` is
  `JqfError`. It is not undefined behavior.
- `Program.free` and `Feed.close` release residency now. The session
  still owns every remaining id until `Session.close`.

## Streaming

- `run_many_streaming` yields chunks of at most 256 KiB, except when
  one encoder write is larger. Concatenating them matches
  `run_many` byte for byte.
- At most `max_pending_chunks` undelivered chunks exist. A slow
  consumer pauses the run. Closing the iterator cancels at the next
  chunk boundary.

## Diagnostics

- Each run or compile clears the handle's diagnostic stream first.
- `diag_dropped` is how many records overflowed the capped buffer
  this operation. Nonzero means the list is incomplete.
