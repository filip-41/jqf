//! Integration coverage for the `--max-memory-bytes` CLI flag: it must reject large input under a tiny ceiling with a
//! resource-class error, admit the same input under a generous ceiling, and — since the ceiling became opt-in — impose
//! NO ceiling when the flag is absent.

use std::io::Write as _;
use std::process::{Command, Output, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

fn run_jqf(args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("jqf spawns");
    // A usage-error child exits WITHOUT reading stdin, closing the pipe mid-write; BrokenPipe is the expected race
    // there, not a test failure (surfaced by the 003 linux-amd64 emulated lane, where the child's exit reliably beats
    // the parent's write).
    if let Err(error) = child.stdin.take().expect("stdin is piped").write_all(input) {
        assert!(
            error.kind() == std::io::ErrorKind::BrokenPipe,
            "input writes to jqf's stdin: {error}"
        );
    }
    child.wait_with_output().expect("jqf runs to completion")
}

/// A single JSON array large enough that its accounted construction cannot fit under a tiny memory ceiling, and
/// trivially small for a run that names no ceiling at all.
fn large_input() -> Vec<u8> {
    let mut input = Vec::new();
    input.push(b'[');
    for index in 0..20_000_u32 {
        if index > 0 {
            input.push(b',');
        }
        input.extend_from_slice(index.to_string().as_bytes());
    }
    input.push(b']');
    input
}

/// A numeric array whose decoded document plus re-encoded output cannot fit a ceiling that still fits the input bytes —
/// the crossing is mid-decode (the input-driven accumulation class of 125 H2), not at the read.
fn large_numeric_input() -> Vec<u8> {
    let mut input = Vec::new();
    input.push(b'[');
    for index in 0..100_000_u32 {
        if index > 0 {
            input.push(b',');
        }
        input.extend_from_slice(index.to_string().as_bytes());
    }
    input.push(b']');
    input
}

/// The incremental-accumulation law: a decode that crosses the ceiling in small steps refuses with the typed resource
/// error (exit 5) — it does not abort (134). The ceiling fits the input buffer but not the document build plus
/// re-encode, so the crossing is an infallible arena growth mid-request, the surface that used to SIGABRT through
/// `handle_alloc_error`.
#[test]
fn accumulative_decode_refuses_with_the_resource_class_not_an_abort() {
    let output = run_jqf(&["--max-memory-bytes", "1000000", "-c", "."], &large_numeric_input());
    assert_ne!(
        output.status.code(),
        Some(134),
        "a decode crossing the ceiling must refuse, not abort: {output:?}"
    );
    assert_eq!(
        output.status.code(),
        Some(5),
        "the refusal is the runtime resource class: {output:?}"
    );
    assert!(output.stdout.is_empty(), "no output on a rejected request");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(
        stderr.starts_with("jqf: error: "),
        "stderr must use the house error frame, got: {stderr:?}"
    );
}

/// The engine-side accumulation law: a program-driven collect that crosses the ceiling (`[range(N)]`, `group_by`
/// scratch) refuses with exit 5, never an abort.
#[test]
fn engine_accumulation_refuses_with_the_resource_class_not_an_abort() {
    let output = run_jqf(&["--max-memory-bytes", "1000000", "[range(2000000)]"], b"null");
    assert_ne!(
        output.status.code(),
        Some(134),
        "a collect crossing the ceiling must refuse, not abort: {output:?}"
    );
    assert_eq!(
        output.status.code(),
        Some(5),
        "the refusal is the runtime resource class: {output:?}"
    );
    assert!(output.stdout.is_empty(), "no output on a rejected request");
}

/// A ceiling below the ledger floor refuses at account creation: the typed memory-limit error, the flag named, never an
/// abort. The floor is the ledger allocation itself (216 bytes here); a 2048-byte ceiling no longer trips
/// infrastructure, so this pins the floor rather than a bootstrap constant.
#[test]
fn tiny_ceiling_on_piped_stdin_refuses_below_the_ledger_floor() {
    let output = run_jqf(&["--max-memory-bytes", "128", "-c", "[.]"], b"{\"a\":1}");
    assert_ne!(
        output.status.code(),
        Some(134),
        "a tight ceiling must refuse, not abort: {output:?}"
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "account creation is the usage class: {output:?}"
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(
        stderr.starts_with("jqf: error: ") && stderr.contains("memory limit exceeded"),
        "stderr must report the memory limit in prose, got: {stderr:?}"
    );
    assert!(
        stderr.contains("--max-memory-bytes"),
        "stderr must name the flag, got: {stderr:?}"
    );
}

#[test]
fn tiny_max_memory_bytes_rejects_a_large_input_with_a_resource_error() {
    let output = run_jqf(&["--max-memory-bytes", "64", "."], &large_input());
    assert!(
        !output.status.success(),
        "a 64-byte ceiling must reject a 20,000-element array: {output:?}"
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    // This assertion used to read `stderr.contains("MemoryBytes")`, which pinned the LEAK: the Rust variant name only
    // appears when the failure is rendered with `Debug`. What it must pin instead is that the reader is told which
    // ceiling stopped them, in prose, with no type syntax.
    assert!(
        stderr.starts_with("jqf: error: ") && stderr.contains("memory limit exceeded"),
        "stderr must report a memory resource-class error, got: {stderr:?}"
    );
    assert!(
        stderr.contains("--max-memory-bytes"),
        "stderr must name the flag that configures the ceiling, got: {stderr:?}"
    );
    for banned in ["MemoryBytes", "LimitExceeded", "limit_kind", "{", "}"] {
        assert!(
            !stderr.contains(banned),
            "stderr leaks Rust type syntax {banned:?}: {stderr:?}"
        );
    }
    assert!(output.stdout.is_empty(), "no output on a rejected request");
}

#[test]
fn generous_max_memory_bytes_produces_normal_output() {
    // `-c` pins the serialization so this memory test asserts on bytes it owns; the default indent belongs to the
    // formatting gate, not to this file.
    let output = run_jqf(&["--max-memory-bytes", "999999999", "-c", "."], b"{\"a\":1}");
    assert!(
        output.status.success(),
        "a generous ceiling must admit a tiny document: {output:?}"
    );
    assert_eq!(output.stdout, b"{\"a\":1}\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn omitting_the_flag_imposes_no_ceiling_at_all() {
    // The exact input the 64-byte ceiling above refuses. Without the flag it must be admitted, because omitting the
    // flag now means NO ceiling rather than the 512 MiB one this bootstrap used to hardcode. A ceiling that refused
    // work every competitor completed was policy, not protection.
    let refused = run_jqf(&["--max-memory-bytes", "64", "-c", "length"], &large_input());
    assert!(
        !refused.status.success(),
        "the 64-byte ceiling must still refuse when it is asked for: {refused:?}"
    );
    let omitted = run_jqf(&["-c", "length"], &large_input());
    assert!(omitted.status.success(), "no flag must impose no ceiling: {omitted:?}");
    assert_eq!(omitted.stdout, b"20000\n");
    assert!(omitted.stderr.is_empty());
}

#[test]
fn duplicate_flag_is_rejected() {
    let output = run_jqf(&["--max-memory-bytes", "1024", "--max-memory-bytes", "2048", "."], b"1");
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("--max-memory-bytes may only be given once"));
}

#[test]
fn missing_flag_value_is_rejected() {
    let output = run_jqf(&["--max-memory-bytes"], b"1");
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("--max-memory-bytes requires a value"));
}

#[test]
fn non_numeric_flag_value_is_rejected() {
    let output = run_jqf(&["--max-memory-bytes", "not-a-number", "."], b"1");
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("--max-memory-bytes value is not a valid nonnegative integer"));
}

/// A string-heavy document whose accounted build grows one text arena far past the point where a tight ceiling refuses
/// to double it — the shape that exposed the amortized-growth cliff.
fn text_heavy_input() -> Vec<u8> {
    let mut input = Vec::new();
    input.push(b'[');
    for index in 0..20_000_u32 {
        if index > 0 {
            input.push(b',');
        }
        input.extend_from_slice(format!(r#"{{"name":"item-{index:06}","sku":"SKU-{index:08}"}}"#).as_bytes());
    }
    input.push(b']');
    input
}

/// Runs one ceiling and returns the wall-clock time it took.
fn timed_run(ceiling: u64, input: &[u8]) -> std::time::Duration {
    let start = std::time::Instant::now();
    let _ = run_jqf(&["--max-memory-bytes", &ceiling.to_string(), "."], input);
    start.elapsed()
}

#[test]
fn a_ceiling_just_under_the_requests_peak_fails_fast_instead_of_thrashing() {
    // THE AMORTIZED-GROWTH CLIFF (`TrackedVec:prepare_growth_backoff` carries the whole diagnosis and the before/after
    // numbers). A refused doubling used to collapse to exact growth and STAY there, so every later append reallocated
    // and recopied the whole arena: an O(n) build became O(n²). On the 10 MB catalog fixture that turned
    // `--max-memory-bytes 138000000 '.'` into a 205-SECOND wait for the same `exit 5` that 130000000 reached in 0.08 s.
    //
    // The assertion is a RATIO against this machine's own successful run, not a wall-clock constant, so it means the
    // same thing on a slow debug build and a fast release one. On the fixture below the pre-fix policy ran 15× and 35×
    // its own baseline at 0.90 and 0.95 of the peak; the fixed policy runs at 0.6×, because a ceiling it cannot finish
    // under is now REJECTED rather than crawled towards.
    let input = text_heavy_input();

    // A ceiling with room to spare: the request completes, and its time is the baseline every constrained run is
    // measured against.
    let baseline = timed_run(64 << 20, &input).max(std::time::Duration::from_millis(5));

    // Two mebibytes cannot hold this fixture. The ceiling must refuse, and the refusal must be a reject — not the O(n²)
    // crawl a stale peak percentage used to hide.
    let ceiling = 2 << 20;
    let start = std::time::Instant::now();
    let output = run_jqf(&["--max-memory-bytes", &ceiling.to_string(), "."], &input);
    let elapsed = start.elapsed();
    assert_ne!(
        output.status.code(),
        Some(0),
        "--max-memory-bytes {ceiling} must refuse this fixture, got {output:?}"
    );
    assert!(
        elapsed < baseline * 8,
        "--max-memory-bytes {ceiling} took {elapsed:?}, more than 8x the unconstrained \
         run's {baseline:?}: the growth policy is thrashing"
    );
}
