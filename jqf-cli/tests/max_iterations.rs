//! Integration coverage for the opt-in iteration ceiling (`--max-iterations`): an unbounded generator grind refuses at
//! the machine resource family (exit 5) with the flag named in stderr and zero stdout; without the flag behavior is
//! byte-identical; a legal large-but-finite run under a generous ceiling completes.

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Runs one jqf request and returns `(exit code, stdout, stderr)`.
fn jqf(arguments: &[&str], stdin: &str) -> (i32, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_jqf"))
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn jqf");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The reported grind: the collect fuses into the never-popping repeat frame, so only the ceiling stops it — and it
/// stops it QUICKLY, with the machine resource refusal naming the flag.
#[test]
fn unbounded_repeat_refuses_quickly_with_the_flag_named() {
    let start = Instant::now();
    let (code, stdout, stderr) = jqf(&["-n", "[repeat(1)] | length", "--max-iterations", "1000000"], "");
    assert!(
        start.elapsed() < Duration::from_secs(30),
        "the ceiling must stop the grind promptly, took {:?}",
        start.elapsed()
    );
    assert_eq!(code, 5, "stderr: {stderr}");
    assert!(stdout.is_empty(), "stdout: {stdout:?}");
    assert!(stderr.contains("--max-iterations"), "stderr: {stderr}");
}

/// Without the flag, a bounded program answers exactly as before — and a generous ceiling changes no bytes either.
#[test]
fn bounded_program_answers_identically_without_and_with_the_flag() {
    const INPUT: &str = r#"{"a":[1,2,3]}"#;
    let (plain_code, plain_out, plain_err) = jqf(&["[.a[]] | length"], INPUT);
    assert_eq!(plain_code, 0, "{plain_err}");
    assert_eq!(plain_out, "3\n");
    assert_eq!(plain_err, "");

    let (capped_code, capped_out, capped_err) = jqf(&["[.a[]] | length", "--max-iterations", "100000000"], INPUT);
    assert_eq!(capped_code, 0, "{capped_err}");
    assert_eq!(capped_out, plain_out);
    assert_eq!(capped_err, "");
}

/// A legal large-but-finite run under a generous ceiling still completes.
#[test]
fn large_finite_run_completes_under_a_generous_ceiling() {
    let (code, stdout, stderr) = jqf(&["-n", "[range(50000)] | add", "--max-iterations", "100000000"], "");
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout.trim(), "1249975000");
    assert_eq!(stderr, "");
}

/// The dial's own spelling laws: a repeated flag is a usage error, and 0 is the documented unlimited spelling (a
/// bounded program answers normally).
#[test]
fn flag_spelling_laws() {
    let (code, _, stderr) = jqf(&["-n", "1", "--max-iterations", "10", "--max-iterations", "20"], "");
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("only be given once"), "{stderr}");

    let (code, stdout, stderr) = jqf(&["-n", "[range(100)] | add", "--max-iterations", "0"], "");
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout.trim(), "4950");
}
