//! A tiny end-to-end smoke of the C ABI from Rust (the Python ctypes path
//! exercises the same entry points).
use std::ptr::from_mut;

fn main() {
    let mut handle = std::ptr::null_mut();
    // SAFETY: every pointer handed to the ABI below is a live local — the
    // handle slot, the program bytes, the input literal, and the heap out
    // buffer all outlive their calls — and the handle passed after `jqf_new`
    // is the one it wrote, used from this single thread and freed once.
    unsafe {
        assert_eq!(jqf_sdk_ffi::jqf_abi_version(), jqf_sdk_ffi::JQF_ABI_VERSION);
        assert_eq!(jqf_sdk_ffi::jqf_new(from_mut(&mut handle)), 0);
        let program = b"[.[] | .price] | add";
        let input = br#"[{"price": 1}, {"price": 2}]"#;
        // Heap, not stack: the out buffer is a caller-sized ABI parameter, and
        // 64 KiB of it does not belong on an example's frame.
        let mut out = vec![0u8; 65536];
        let written = jqf_sdk_ffi::jqf_run(
            handle,
            program.as_ptr(),
            program.len(),
            input.as_ptr(),
            input.len(),
            out.as_mut_ptr(),
            out.len(),
        );
        assert!(written >= 0, "run failed");
        let written = usize::try_from(written).expect("a non-negative byte count");
        println!("output: {}", String::from_utf8_lossy(&out[..written]));
        let count = jqf_sdk_ffi::jqf_diag_count(handle);
        println!("diag count: {count}");
        for i in 0..count {
            let mut code = 0u16;
            let mut revision = 0u16;
            let mut class = 0 as std::ffi::c_char;
            let mut severity = 0 as std::ffi::c_char;
            let mut catchable = 0u8;
            let mut caught = 0u32;
            let mut step_index = 0u32;
            let mut input_ordinal = 0u64;
            let mut byte_offset = 0u64;
            let mut halt_status = 0i32;
            let mut kind = std::ptr::null_mut();
            let mut operand = std::ptr::null_mut();
            let mut payload = std::ptr::null_mut();
            let rc = jqf_sdk_ffi::jqf_diag_get(
                handle,
                i,
                from_mut(&mut code),
                from_mut(&mut revision),
                from_mut(&mut class),
                from_mut(&mut severity),
                from_mut(&mut catchable),
                from_mut(&mut caught),
                from_mut(&mut step_index),
                from_mut(&mut input_ordinal),
                from_mut(&mut byte_offset),
                from_mut(&mut halt_status),
                from_mut(&mut kind),
                from_mut(&mut operand),
                from_mut(&mut payload),
            );
            assert_eq!(rc, 0);
            let kind_s = if kind.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(kind).to_string_lossy().into_owned()
            };
            println!("diag[{i}]: code={code} kind={kind_s}");
            if !kind.is_null() {
                jqf_sdk_ffi::jqf_diag_free_text(kind);
            }
            if !operand.is_null() {
                jqf_sdk_ffi::jqf_diag_free_text(operand);
            }
            if !payload.is_null() {
                jqf_sdk_ffi::jqf_diag_free_text(payload);
            }
        }
        jqf_sdk_ffi::jqf_free(handle);
    }
}
