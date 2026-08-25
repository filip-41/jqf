//! A loop SOURCE the codec resolves must not be able to delete the fold's init.
//!
//! The codec pushdown hands the executor one located value for a binder/loop source it can resolve completely. An
//! OPTIONAL step breaks the "one value" promise — a suppressed type mismatch locates nothing — and a located nothing
//! reads as "no document", so the program never runs. For a `reduce` that is a wrong answer rather than a missing
//! optimization: an empty source still emits the init.
//!
//! These are end-to-end because the defect lives in the seam between the analysis and the codec route; only a
//! whole-process run engages it. The matching unit assertion is `an_optional_loop_source_declines_the_pushdown` in
//! `jqf-engine/src/analysis/demand.rs`.

use std::io::Write as _;
use std::process::{Command, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// Runs `program` over `input` and returns stdout, asserting a clean exit.
fn run(program: &str, input: &[u8]) -> String {
    let mut child = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(["-c", program])
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
    let output = child.wait_with_output().expect("jqf runs to completion");
    assert!(output.status.success(), "{program} must exit cleanly: {output:?}");
    String::from_utf8(output.stdout).expect("stdout is UTF-8")
}

#[test]
fn an_optional_loop_source_that_locates_nothing_still_emits_the_init() {
    // `.a` on an array is a type mismatch; the `?` suppresses it, leaving the source empty. answers `0` — the init,
    // unchanged.
    assert_eq!(run("reduce .a? as $x (0; . + 1)", b"[1,2,3]"), "0\n");
    // The init flows on down the pipe, which is where the deletion was loudest: the whole program answered nothing.
    assert_eq!(run("reduce .a? as $x (0; . + 1) | . + 100", b"[1,2,3]"), "100\n");
}

#[test]
fn the_suppressed_mismatch_may_sit_anywhere_in_the_located_path() {
    // The pushdown resolves the WHOLE static path, so the miss can happen at any step of it — a deeper key and an index
    // step reach the same seam.
    assert_eq!(run("reduce .a.b? as $x (0; . + 1)", br#"{"a":5}"#), "0\n");
    assert_eq!(run("reduce .a[0]? as $x (0; . + 1)", br#"{"a":{}}"#), "0\n");
    // A scalar document has no key at all: `.a?` locates nothing from the root.
    assert_eq!(run("reduce .a? as $x (0; . + 1)", b"\"s\""), "0\n");
}

#[test]
fn a_located_optional_source_still_folds_what_it_finds() {
    // The guard declines a pushdown; it must not change an answer the pushdown was computing correctly. A present path
    // folds exactly as before.
    assert_eq!(run("reduce .a? as $x (0; . + $x)", br#"{"a":7}"#), "7\n");
    assert_eq!(run("reduce .a.b? as $x (0; . + $x)", br#"{"a":{"b":9}}"#), "9\n");
    // A missing KEY is not a mismatch: it locates `null`, which is one item.
    assert_eq!(run("reduce .a? as $x (0; . + 1)", b"{}"), "1\n");
}
