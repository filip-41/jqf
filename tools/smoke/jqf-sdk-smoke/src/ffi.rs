//! FFI correct-core receipt: table-driven rows through the C ABI.
//!
//! Pins `jqf_new` / `jqf_run` / `jqf_compile_args` / `jqf_run_compiled` /
//! `jqf_new_limited` — never a jqf-sdk private path. Success rows must match
//! the CLI's bytes. Sibling of the SDK batteries in this crate; no shared
//! harness types.

fn ffi_diag_halt_status(handle: *mut core::ffi::c_void, index: u32) -> (i32, u8) {
    let mut code = 0u16;
    let mut revision = 0u16;
    let mut record_class = 0 as core::ffi::c_char;
    let mut severity = 0 as core::ffi::c_char;
    let mut catchable = 0u8;
    let mut caught = 0u32;
    let mut step_index = 0u32;
    let mut input_ordinal = 0u64;
    let mut byte_offset = 0u64;
    let mut halt_status = 0i32;
    let mut kind = core::ptr::null_mut();
    let mut operand = core::ptr::null_mut();
    let mut payload = core::ptr::null_mut();
    // SAFETY: `handle` is a live `jqf_new` pointer; every out-param is a
    // stack slot of the declared type.
    let rc = unsafe {
        jqf_sdk_ffi::jqf_diag_get(
            handle,
            index,
            &raw mut code,
            &raw mut revision,
            &raw mut record_class,
            &raw mut severity,
            &raw mut catchable,
            &raw mut caught,
            &raw mut step_index,
            &raw mut input_ordinal,
            &raw mut byte_offset,
            &raw mut halt_status,
            &raw mut kind,
            &raw mut operand,
            &raw mut payload,
        )
    };
    assert_eq!(rc, 0, "jqf_diag_get failed");
    // SAFETY: the three pointers came from this `jqf_diag_get` call.
    unsafe {
        jqf_sdk_ffi::jqf_diag_free_text(kind);
        jqf_sdk_ffi::jqf_diag_free_text(operand);
        jqf_sdk_ffi::jqf_diag_free_text(payload);
    }
    (halt_status, catchable)
}

fn ffi_diag_payload(handle: *mut core::ffi::c_void, index: u32) -> Vec<u8> {
    // SAFETY: `handle` is live; `(NULL, 0)` is the documented sizing probe.
    let required = unsafe {
        jqf_sdk_ffi::jqf_diag_get_text(
            handle,
            index,
            jqf_sdk_ffi::JQF_DIAG_TEXT_PAYLOAD,
            core::ptr::null_mut(),
            0,
        )
    };
    if required <= 0 {
        return Vec::new();
    }
    let mut buf = vec![0u8; usize::try_from(required).unwrap()];
    // SAFETY: `buf` is valid for `buf.len()` bytes; `handle` is live.
    let n = unsafe {
        jqf_sdk_ffi::jqf_diag_get_text(
            handle,
            index,
            jqf_sdk_ffi::JQF_DIAG_TEXT_PAYLOAD,
            buf.as_mut_ptr(),
            buf.len(),
        )
    };
    assert_eq!(n, required, "payload length drifted between probe and copy");
    buf
}

/// FFI correct-core receipt: table-driven rows through the C ABI
/// (`jqf_new` / `jqf_run` / `jqf_compile_args` / `jqf_run_compiled` /
/// `jqf_new_limited`), never a jqf-sdk private path. Pins the shared input cursor,
/// `$ENV`, adjacent-value input, `$ARGS` bindings, halt/error diagnostics,
/// and the encode-options slice. Success rows must match the CLI's bytes.
#[expect(
    clippy::too_many_lines,
    reason = "the row table and its driver are one receipt: each row is one sentence of the scope ruling"
)]
pub(crate) fn assert_ffi_correct_core() {
    use std::ffi::c_void;
    use std::ptr;

    /// One pinned capability row: program, input, expected output (None =
    /// a terminal failure).
    type FfiRow = (&'static str, &'static [u8], Option<&'static [u8]>);
    let rows: &[FfiRow] = &[
        // A1: `[inputs]` skips the first value (`.`) and collects the rest.
        ("[inputs]", b"1\n2\n3\n", Some(b"[2,3]\n")),
        // A2: `input` reads the next value.
        ("input", b"1\n2\n", Some(b"2\n")),
        // A3: `$ENV` is the host snapshot, never an empty object.
        ("$ENV | type", b"null", Some(b"\"object\"\n")),
        // A4: adjacent values are the default input model.
        (".", b"1 2 3", Some(b"1\n2\n3\n")),
        // A5: a request with no bindings still has empty `$ARGS`.
        ("$ARGS", b"null", Some(b"{\"positional\":[],\"named\":{}}\n")),
        // A6: `halt_error` keeps its status and message on the last record.
        ("halt_error(5)", b"\"boom\"", None),
        // A7: `error({"code":42})` keeps the object recoverable.
        ("error({\"code\":42})", b"null", None),
    ];

    // A fresh handle per row: the rows are independent capabilities and a
    // handle is one consumer.
    for (program, input, expected) in rows {
        let mut handle: *mut c_void = ptr::null_mut();
        // SAFETY: `ptr::from_mut(&mut handle)` is a live `*mut *mut c_void` out-slot.
        let rc = unsafe { jqf_sdk_ffi::jqf_new(ptr::from_mut(&mut handle)) };
        assert_eq!(rc, 0, "jqf_new failed");
        let mut out = vec![0u8; 65536];
        // SAFETY: `program`/`input` are readable for their lengths; `out` is
        // valid for its capacity; `handle` is live.
        let written = unsafe {
            jqf_sdk_ffi::jqf_run(
                handle,
                program.as_ptr(),
                program.len(),
                input.as_ptr(),
                input.len(),
                out.as_mut_ptr(),
                out.len(),
            )
        };
        if let Some(expected) = expected {
            let written = usize::try_from(written).expect("jqf_run length");
            assert!(
                written <= out.len(),
                "{program:?} truncated: required {written} cap {}",
                out.len()
            );
            assert_eq!(&out[..written], *expected, "{program:?} must answer as under the CLI");
        } else {
            assert_eq!(written, -1, "{program:?} must be a terminal failure");
            // SAFETY: `handle` is live from this row's `jqf_new`.
            let count = unsafe { jqf_sdk_ffi::jqf_diag_count(handle) };
            assert!(count >= 1, "{program:?} must retain a failure record");
            let index = count - 1;
            let payload = ffi_diag_payload(handle, index);
            if *program == "halt_error(5)" {
                let (status, _) = ffi_diag_halt_status(handle, index);
                assert_eq!(status, 5, "halt_error must keep status 5");
                assert!(
                    payload.windows(4).any(|w| w == b"boom"),
                    "halt_error must keep the message, got {payload:?}"
                );
            } else if program.starts_with("error(") {
                let (_, catchable) = ffi_diag_halt_status(handle, index);
                assert_ne!(
                    catchable, 0,
                    "error object must stay recoverable, catchable={catchable}"
                );
                assert!(
                    payload.windows(9).any(|w| w == b"\"code\":42") || payload.windows(2).any(|w| w == b"42"),
                    "error object must keep code 42, got {payload:?}"
                );
            }
        }
        // SAFETY: `handle` is live, freed exactly once.
        unsafe { jqf_sdk_ffi::jqf_free(handle) };
    }

    // Host data reaches the program as a JSON constant; `$ARGS` keeps the CLI shape.
    let mut handle: *mut c_void = ptr::null_mut();
    // SAFETY: `handle` is a live local slot.
    assert_eq!(unsafe { jqf_sdk_ffi::jqf_new(ptr::from_mut(&mut handle)) }, 0);
    let program = "$x + 1";
    let name = c"x";
    let value = b"41";
    let names = [name.as_ptr().cast()];
    let values = [value.as_ptr()];
    let lengths = [2usize];
    let mut id = 0u32;
    // SAFETY: the parallel arrays have one entry each (a live C string name
    // and a readable (ptr, len) value); `id` is a live local slot.
    let rc = unsafe {
        jqf_sdk_ffi::jqf_compile_args(
            handle,
            program.as_ptr(),
            program.len(),
            1,
            names.as_ptr(),
            values.as_ptr(),
            lengths.as_ptr(),
            ptr::from_mut(&mut id),
        )
    };
    assert_eq!(rc, 0, "jqf_compile_args failed");
    let mut out = vec![0u8; 65536];
    // SAFETY: `id` names a live program on `handle`; `out` is valid.
    let written =
        unsafe { jqf_sdk_ffi::jqf_run_compiled(handle, id, b"null".as_ptr(), 4, out.as_mut_ptr(), out.len()) };
    let written = usize::try_from(written).expect("jqf_run_compiled length");
    assert!(written <= out.len(), "bound program truncated: required {written}");
    assert_eq!(&out[..written], b"42\n", "a binding must reach the program as a value");
    // SAFETY: `handle` is live, freed exactly once.
    unsafe { jqf_sdk_ffi::jqf_free(handle) };

    // A raw-string request writes a ROOT string verbatim.
    let mut handle: *mut c_void = ptr::null_mut();
    let encode = jqf_sdk_ffi::JqfEncodeOptions {
        indent: -1,
        raw_strings: 1,
        sort_keys: 0,
        ascii_output: 0,
        raw_output_nul: 0,
    };
    // SAFETY: `handle` is a live local slot; `encode` is initialized.
    assert_eq!(
        unsafe { jqf_sdk_ffi::jqf_new_limited(ptr::null(), ptr::from_ref(&encode), ptr::from_mut(&mut handle),) },
        0
    );
    let mut out = vec![0u8; 65536];
    // SAFETY: `handle` is live from `jqf_new_limited`; program/input/out are
    // valid for the lengths passed.
    let written = unsafe {
        jqf_sdk_ffi::jqf_run(
            handle,
            b".".as_ptr(),
            1,
            // `"hi"` is four bytes; the length must be exact — an off-by-one
            // leaks the literal's NUL into the decode.
            b"\"hi\"".as_ptr(),
            b"\"hi\"".len(),
            out.as_mut_ptr(),
            out.len(),
        )
    };
    let written = usize::try_from(written).expect("raw-string jqf_run length");
    assert!(written <= out.len(), "raw-string run truncated: required {written}");
    assert_eq!(&out[..written], b"hi\n", "raw strings must reach the encoder");
    // SAFETY: `handle` is live, freed exactly once.
    unsafe { jqf_sdk_ffi::jqf_free(handle) };
}
