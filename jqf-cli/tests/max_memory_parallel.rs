//! Integration coverage for 125 H1: the memory ceiling must bind on the PARALLEL record lane, and a refused allocation
//! inside a worker must fail the MORSEL (yield to serial), never abort the process.
//!
//! Before H1 a worker thread ran with no ambient scope, so the counting allocator charged its allocations to nothing
//! and a memory-hungry program SAILED PAST the ceiling exactly where the parallel lane is the highest-memory lane.
//! After H1 each worker installs the child ledger opened from its grant as its thread's ambient account, so the grant
//! envelope refuses the program inside the worker, the morsel yields to serial, and the serial redrive refuses under
//! the parent ceiling with the machine error class.
//!
//! The other refusal shape is the GRANT ENVELOPE itself, under the DEFAULT ceiling: a record too large for one worker's
//! working component — a CSV field split that must grow past the grant — makes the counted allocator refuse a plain
//! `Vec` growth, which pre-fix aborted the process with `memory allocation of N bytes failed` (the 133 W3-exit
//! record-parallel abort). The field-split growth is now fallible, so the worker fails cleanly and the relay yields to
//! serial, which publishes serial's exact bytes. `a_giant_csv_record_fails_the_worker_and_yields` pins that law.
//!
//! The input is fed as a SEEKABLE stdin redirect (a temp file), because a pipe would stream serially and the test would
//! not exercise the parallel lane at all. The same request under a generous ceiling completes, which is what proves the
//! refusal is the ceiling and not a broken lane.

use std::fs;
use std::process::{Command, Output, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// A small NDJSON input of FAT records: large enough that the parallel lane engages (two workers, > 128 KiB morsels)
/// but small enough that a regression that sails past the ceiling completes in well under a second (a dozen
/// million-element collects, not two hundred thousand).
fn fat_records() -> Vec<u8> {
    let mut input = Vec::new();
    for index in 0..12_u32 {
        input.extend_from_slice(format!(r#"{{"payload":"{}","id":{index}}}"#, "x".repeat(32 * 1024)).as_bytes());
        input.push(b'\n');
    }
    input
}

/// Runs jqf with the NDJSON input redirected from a temp file, so the stdin descriptor is a seekable regular file and
/// the record route's parallel planner engages.
fn run_jqf(args: &[&str], input: &[u8]) -> Output {
    let dir = std::env::temp_dir().join(format!("jqf-h1-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("input.ndjson");
    fs::write(&path, input).expect("write input");
    let file = fs::File::open(&path).expect("open input");
    let child = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(args)
        .stdin(Stdio::from(file))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("jqf spawns");
    let output = child.wait_with_output().expect("jqf runs to completion");
    let _ = fs::remove_dir_all(&dir);
    output
}

/// Every record collects a million integers and publishes the first — a ~40 MB transient per record with a two-byte
/// publication, exactly the shape the ceiling must refuse rather than sail past.
const COLLECT_ONE: &str = "[range(0;1000000)] | .[0]";

#[test]
fn the_parallel_lane_refuses_under_a_ceiling_instead_of_sailing_past() {
    let output = run_jqf(
        &[
            "--input-format",
            "ndjson",
            "--parallel",
            "--workers",
            "2",
            "--max-memory-bytes",
            "4194304",
            "-c",
            COLLECT_ONE,
        ],
        &fat_records(),
    );
    assert_eq!(
        output.status.code(),
        Some(5),
        "a parallel lane that trips its ceiling must refuse with the machine \
         class (exit 5), got {:?}; pre-H1 this request sailed past the ceiling \
         with exit 0",
        output.status
    );
    assert!(output.stdout.is_empty(), "a refused request must publish zero bytes");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(
        stderr.contains("allocation failed") || stderr.contains("memory"),
        "stderr must report the resource-class refusal, got: {stderr:?}"
    );
}

#[test]
fn the_same_parallel_lane_completes_under_a_generous_ceiling() {
    let output = run_jqf(
        &[
            "--input-format",
            "ndjson",
            "--parallel",
            "--workers",
            "2",
            "--max-memory-bytes",
            "999999999",
            "-c",
            COLLECT_ONE,
        ],
        &fat_records(),
    );
    assert!(
        output.status.success(),
        "a generous ceiling must admit the same request: {:?}",
        output.status
    );
    assert_eq!(
        output.stdout,
        b"0\n".repeat(12),
        "each record publishes its collected array's first element"
    );
}

/// A CSV stream whose LAST framed record is a quoted field that never closes, so it runs to end of input (~250 KiB of
/// clean rows swallowed inside one quote). The field split of that record must grow its scratch past the worker's grant
/// — the 133 record-parallel abort shape — and fail the morsel instead of aborting the process.
fn giant_unterminated_csv() -> Vec<u8> {
    let mut input = Vec::new();
    for index in 0..30000_u32 {
        input.extend_from_slice(format!("i{index},v{index}\n").as_bytes());
    }
    // Cut just AFTER the first morsel boundary (a 128 KiB morsel window for this input at `--workers 4`): the morsel
    // that holds the unclosed quote then contains only a handful of clean rows before it, so its published output stays
    // inside the worker's output buffer and the drive reaches the giant record. That record's field split must grow its
    // scratch past the worker's ~192 KiB working component, which is the pre-fix abort condition — a shorter giant
    // record, or one whose morsel overflows the output buffer first, would fail the decode without ever crossing the
    // grant, and the test would pass for the wrong reason.
    let cut = input
        .iter()
        .enumerate()
        .filter(|&(_, &byte)| byte == b'\n')
        .nth(12000)
        .map_or(0, |(index, _)| index + 1);
    let mut giant = Vec::new();
    giant.extend_from_slice(&input[..cut]);
    giant.extend_from_slice(b"\"unterminated\n");
    giant.extend_from_slice(&input[cut..]);
    giant
}

/// Runs jqf with a CSV input redirected from its OWN temp file. The shared `run_jqf` helper writes every input to one
/// `input.ndjson` path, which the NDJSON tests in this file also read — cargo runs the three tests in parallel threads,
/// so the CSV input must not overwrite theirs (and theirs must not overwrite this one).
fn run_jqf_csv(args: &[&str], input: &[u8]) -> Output {
    let dir = std::env::temp_dir().join(format!(
        "jqf-133-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("csv")
    ));
    fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("input.csv");
    fs::write(&path, input).expect("write input");
    let file = fs::File::open(&path).expect("open input");
    let child = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(args)
        .stdin(Stdio::from(file))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("jqf spawns");
    let output = child.wait_with_output().expect("jqf runs to completion");
    let _ = fs::remove_dir_all(&dir);
    output
}

/// The differential's aborting row, pinned: a parallel CSV request whose giant unterminated record overflows the
/// worker's grant must complete with the SAME bytes and exit class as serial — the worker fails, the relay yields to
/// serial — and must never die with a signal. Pre-fix this request `SIGABRT`ed (the counted allocator refused a plain
/// 256 KiB scratch growth inside `split_fields_scalar`, and the backtrace allocations failed too); post-fix the refusal
/// is a codec `AllocationFailure` that fails the morsel.
#[test]
fn a_giant_csv_record_fails_the_worker_and_yields() {
    let input = giant_unterminated_csv();
    let serial = run_jqf_csv(&["--input-format", "csv", "--no-parallel", "."], &input);
    assert_eq!(
        serial.status.code(),
        Some(5),
        "serial rejects the unterminated quote with exit 5: {:?}",
        serial.status
    );
    let parallel = run_jqf_csv(&["--input-format", "csv", "--parallel", "--workers", "4", "."], &input);
    assert_eq!(
        parallel.status.code(),
        Some(5),
        "the parallel lane must complete with serial's exit class (5), not a \
         signal: got {:?}; pre-fix the worker aborted the process",
        parallel.status
    );
    assert_eq!(
        parallel.stdout, serial.stdout,
        "the parallel lane must publish serial's exact bytes after the \
         yield-to-serial"
    );
}
