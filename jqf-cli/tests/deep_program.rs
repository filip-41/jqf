//! Ledger item 27: deeply nested PROGRAM text must not crash the process.
//!
//! ~700 nested constructors used to overflow the stack in parse/lowering (process abort, exit 134). jq parses thousands
//! of levels fine; jqf must match that instead of dying, since a service accepting untrusted filters turns a crash into
//! an availability bug.

use std::io::Write as _;
use std::process::{Command, Output, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

fn run_jqf(program: &str, input: &[u8]) -> Output {
    run_jqf_args(&[program], input)
}

fn run_jqf_args(args: &[&str], input: &[u8]) -> Output {
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

/// Asserts that `program` is REFUSED with a diagnostic instead of aborting.
///
/// The distinction the whole file exists for: a refusal is a normal jqf diagnostic on stderr with a nonzero exit code,
/// and an abort is the Rust runtime's `has overflowed its stack` with no exit code at all (signal 6, which the shell
/// reports as 134).
///
/// The program goes through `-f FILE`, which is how the reported repro reached the compiler and the only way it can:
/// the deeper shapes here are megabytes of source, well past what any `argv` will carry — which is also why sizing the
/// request stack against the `argv` cap was never a bound at all.
fn assert_refused_not_aborted(program: &str, shape: &str) {
    let mut path = std::env::temp_dir();
    path.push(format!("jqf-deep-program-{shape}-{}.jq", std::process::id()));
    std::fs::write(&path, program).expect("the program file writes");
    let output = run_jqf_args(&["-f", path.to_str().expect("a UTF-8 temp path")], b"null\n");
    std::fs::remove_file(&path).expect("the program file is removed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("overflowed its stack"),
        "{shape}: the program guard must fire before the Rust stack does, got: {stderr:?}"
    );
    assert_eq!(
        output.status.code(),
        Some(3),
        "{shape}: a program jqf refuses exits 3 (jq's compile-error class), got: {stderr:?}"
    );
    assert!(
        stderr.contains("nesting depth limit exceeded"),
        "{shape}: the refusal names the nesting ceiling, got: {stderr:?}"
    );
}

#[test]
fn a_thousand_nested_constructors_compile_and_run() {
    // parses 3,000 nested brackets; jqf used to abort at ~700.
    let program = format!("{}1{}", "[".repeat(1_000), "]".repeat(1_000));
    let output = run_jqf(&program, b"");
    assert!(
        output.status.success(),
        "deep program must compile and run, got: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("overflow"),
        "a deep program must never overflow the stack, got: {stderr:?}"
    );
}

#[test]
fn a_two_scan_count_takes_the_count_sum_route() {
    // The two-traversal count (`[.catalog[].id,.catalog[].price] | length`) used to pay the whole-document decode; the
    // count-sum row serves it via two structure-only walks. The bytes must match the forced floor, and the route must
    // be observable as the count-sum rung.
    let input = br#"{"catalog":[{"id":1,"price":1.5},{"id":2,"price":2.5}]}"#;
    let output = run_jqf("[.catalog[].id, .catalog[].price] | length", input);
    assert!(output.status.success(), "count-sum succeeds: {output:?}");
    assert_eq!(output.stdout, b"4\n");
    assert!(output.stderr.is_empty());
    let floor = run_jqf("[.][0] | ([.catalog[].id, .catalog[].price] | length)", input);
    assert_eq!(output.stdout, floor.stdout);
    // A probe violation (a non-object element) downgrades to the floor, which reproduces jq's error byte for byte.
    let bad = br#"{"catalog":[{"id":1},"oops"]}"#;
    let violated = run_jqf("[.catalog[].id, .catalog[].price] | length", bad);
    let floor_bad = run_jqf("[.][0] | ([.catalog[].id, .catalog[].price] | length)", bad);
    assert_eq!(violated.stdout, floor_bad.stdout);
    assert_eq!(violated.status.code(), floor_bad.status.code());
}

#[test]
fn program_text_past_the_nesting_ceiling_is_refused_not_aborted() {
    // Probe sweep 037 finding 1: 50,000 nested brackets aborted the process (`fatal runtime error: stack overflow`,
    // exit 134) where jq reports `memory exhausted` and exits 3. The input side already guarded its recursion; the
    // program side did not, at any of the layers that recurse.
    //
    // Every shape here nests one of the parser's recursive descents, and each is one past the 10,000-level program
    // ceiling. One past is enough: 50_000 was the original abort repro and spends a minute on debug CI just building
    // and dropping trees the guard already refused.
    const OVER: usize = 10_001;
    for (shape, program) in [
        ("brackets", format!("{}{}", "[".repeat(OVER), "]".repeat(OVER))),
        ("parens", format!("{}1{}", "(".repeat(OVER), ")".repeat(OVER))),
        ("objects", format!("{}1{}", "{\"a\":".repeat(OVER), "}".repeat(OVER))),
        // The `null|` prefix is not decoration: a program that STARTS with `-` is read as a flag, not as a filter.
        ("negation", format!("null|{}1", "-".repeat(OVER))),
        ("try", format!("{}1", "try ".repeat(OVER))),
        // NESTED defs — each definition's body is the next definition. Flat sequential defs (`def f: 1; def f: 1; …`)
        // are siblings of the program unit, not tree levels, so they charge no depth and compile (jq agrees until its
        // own parser's memory artifact at ~10k items); only the body-nested form recurses.
        (
            "definitions",
            format!("{}1{}", "def f: ".repeat(OVER), "; 1".repeat(OVER)),
        ),
        ("bindings", format!("{}1", "1 as $x | ".repeat(OVER))),
        (
            "conditionals",
            format!("{}1{}", "if true then ".repeat(OVER), " else 2 end".repeat(OVER)),
        ),
        (
            "folds",
            format!("{}1{}", "reduce empty as $x (0; ".repeat(OVER), ")".repeat(OVER)),
        ),
        (
            "interpolations",
            format!("{}1{}", "\"\\(".repeat(OVER), ")\"".repeat(OVER)),
        ),
        (
            "patterns",
            format!(". as {}$x{} | 1", "[".repeat(OVER), "]".repeat(OVER)),
        ),
    ] {
        assert_refused_not_aborted(&program, shape);
    }
}

#[test]
fn flat_operator_chains_past_the_ceiling_are_refused_not_aborted() {
    // The other half of finding 1, and the one a parse-recursion guard alone does NOT catch: these chains parse
    // ITERATIVELY (the operator helpers use an explicit pending vector), so the parser never recurses — but the tree
    // they build is one level deep per operator, and both the lowering walk and the AST's own recursive `Drop` then run
    // at that depth. 200,000 links aborted the process before the chain guard counted them.
    const OVER: usize = 10_001;
    for (shape, program) in [
        ("pipe", format!("1{}", "|1".repeat(OVER))),
        ("comma", format!("1{}", ",1".repeat(OVER))),
        ("alternative", format!("1{}", "//1".repeat(OVER))),
        ("addition", format!("1{}", "+1".repeat(OVER))),
    ] {
        assert_refused_not_aborted(&program, shape);
    }
}

#[test]
fn programs_at_the_nesting_ceiling_still_compile_and_run() {
    // The twin that makes the refusals above mean something: the ceiling is reachable, so a refusal one level past it
    // is the GUARD firing and not the stack running out early. Parentheses are the cleanest carrier — they nest the
    // parser without nesting the VALUE, so nothing but the program guard is under test.
    let program = format!("{}1{}", "(".repeat(9_999), ")".repeat(9_999));
    let output = run_jqf(&program, b"null\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "9,999 nested groups are inside the ceiling and must run, got: {stderr:?}"
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "1\n");
}

#[test]
fn a_deep_alternative_chain_compiles_linear_arena() {
    // Ledger item 29: a `?//` chain nested in its own body used to copy the body once per arm, squaring the arena (421
    // authored bytes -> 2.2 GiB). The body is now shared behind a `ChainBody` barrier; depth 40 must compile and run on
    // the big-stack request thread, and the CLI must not take quadratic time to do it.
    let mut program = String::from(".");
    for _ in 0..40 {
        program.insert_str(0, ". as [$a] ?// $a | (");
        program.push(')');
    }
    let output = run_jqf(&program, b"");
    assert!(
        output.status.success(),
        "a 40-deep alternative chain must compile and run, got: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("lowering bound") && !stderr.contains("overflow"),
        "the chain must neither hit the arena bound nor overflow, got: {stderr:?}"
    );
}
