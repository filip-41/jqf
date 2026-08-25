# jqf-sdk-ffi

The C ABI for jqf's SDK: run a program over bytes and read structured
diagnostics. Built as a `cdylib` for language bindings.

This crate links `std`. It depends on `jqf-sdk` for the request, `jqf-engine`
for compile, `jqf-runtime` for feed, `jqf-resource` for the ledger, and the
codec crates the handle registers. It does not parse a format grammar or
open files.

What it has:

- `jqf_abi_version` — bindings check this at load and refuse a mismatch
- `jqf_new` / `jqf_new_limited` / `jqf_free` — one handle, one thread
- `jqf_compile` / `jqf_compile_args` / `jqf_program_free` — compile once
- `jqf_run` / `jqf_run_compiled` / `jqf_run_sequence` /
  `jqf_run_sequence_compiled` — snprintf-style publish into a caller buffer
- `jqf_run_sequence_streaming` — the same drive, chunks to a host callback
- `jqf_feed_open` / `jqf_feed_push` / `jqf_feed_poll` / `jqf_feed_finish` /
  `jqf_feed_close` — push input in chunks
- `jqf_diag_count` / `jqf_diag_dropped` / `jqf_diag_get` /
  `jqf_diag_get_text` / `jqf_diag_free_text` — last operation's records
- `jqf_run_errors_count` / `jqf_run_error_get` — published error values
- `include/jqf_sdk_ffi.h` — the checked-in C header

No engine pointer ever escapes. Failure is `-1`. A required byte count
greater than the offered capacity means the buffer holds only a prefix.
C-string diagnostic out-params drop interior NULs (NULL pointer);
length-carrying getters keep them.

```rust
use std::ptr::from_mut;

let mut handle = std::ptr::null_mut();
unsafe {
    assert_eq!(jqf_sdk_ffi::jqf_abi_version(), jqf_sdk_ffi::JQF_ABI_VERSION);
    assert_eq!(jqf_sdk_ffi::jqf_new(from_mut(&mut handle)), 0);
    let program = b"1 + 1";
    let input = b"null";
    let mut out = [0u8; 32];
    let written = jqf_sdk_ffi::jqf_run(
        handle,
        program.as_ptr(),
        program.len(),
        input.as_ptr(),
        input.len(),
        out.as_mut_ptr(),
        out.len(),
    );
    assert!(written >= 0);
    jqf_sdk_ffi::jqf_free(handle);
}
```

## Contracts

See [`CONTRACTS.md`](CONTRACTS.md) for ABI, handle, and buffer invariants.
