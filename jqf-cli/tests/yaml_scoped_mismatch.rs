//! The YAML scoped route reports the same type mismatch its own floor does.
//!
//! When the demand analysis pushes a static path into the codec, the YAML codec navigates the graph itself and
//! publishes a negative observation instead of a located node. A MISMATCH — a member step on a sequence, an index step
//! on a mapping, any step on a scalar — must carry the kind of the value it was applied to: `null` is the one kind jq
//! lets a member step index, so a mismatch that reports `Null` reads as legal and answers `null` where the
//! whole-document floor raises.
//!
//! Each case is therefore asserted against the floor as well as against a literal, by running the same program twice:
//! once as written (pushed down) and once behind `. as $z |`, which forces the whole-document route. The two must agree
//! — that agreement is the property, and a fixed `actual_type` broke it silently.

use std::io::Write as _;
use std::process::{Command, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// Runs `program` over `input` as YAML, returning `(stdout, stderr)`.
fn run(program: &str, input: &str) -> (String, String) {
    let mut child = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(["-c", "--input-format", "yaml", program])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("jqf spawns");
    // A usage-error child exits WITHOUT reading stdin, closing the pipe mid-write; BrokenPipe is the expected race
    // there, not a test failure (surfaced by the 003 linux-amd64 emulated lane, where the child's exit reliably beats
    // the parent's write).
    if let Err(error) = child.stdin.take().expect("stdin is piped").write_all(input.as_bytes()) {
        assert!(
            error.kind() == std::io::ErrorKind::BrokenPipe,
            "input writes to jqf's stdin: {error}"
        );
    }
    let output = child.wait_with_output().expect("jqf runs to completion");
    (
        String::from_utf8(output.stdout).expect("stdout is UTF-8"),
        String::from_utf8(output.stderr).expect("stderr is UTF-8"),
    )
}

/// Asserts the pushed-down route and the whole-document floor agree, and that the shared answer names `expected` in its
/// error text.
fn scoped_matches_floor(program: &str, input: &str, expected: &str) {
    let (scoped_out, scoped_err) = run(program, input);
    let (floor_out, floor_err) = run(&format!(". as $z | {program}"), input);
    assert_eq!(
        (scoped_out.as_str(), scoped_err.as_str()),
        (floor_out.as_str(), floor_err.as_str()),
        "{program} must answer the same pushed down as it does at the floor"
    );
    assert!(
        scoped_err.contains(expected),
        "{program} must report {expected}, got {scoped_err:?}"
    );
}

#[test]
fn a_member_step_on_a_sequence_raises_with_the_sequence_named() {
    scoped_matches_floor(".a", "- 1\n- 2\n- 3\n", "Cannot index array with string");
    // A deeper path fails at its FIRST step, exactly as the floor does.
    scoped_matches_floor(".a.b", "- 1\n- 2\n- 3\n", "Cannot index array with string");
}

#[test]
fn an_index_step_on_a_mapping_raises_with_the_mapping_named() {
    scoped_matches_floor(".[0]", "a: 1\n", "Cannot index object with number");
}

#[test]
fn a_step_on_a_scalar_names_the_scalars_resolved_kind() {
    // The kind of a YAML scalar is a schema question, not a node-shape one: one node shape, four answers.
    scoped_matches_floor(".a", "5\n", "Cannot index number with string");
    scoped_matches_floor(".a", "\"s\"\n", "Cannot index string with string");
    scoped_matches_floor(".a", "true\n", "Cannot index boolean with string");
}

#[test]
fn the_two_legal_negative_observations_still_answer_null() {
    // jq lets a member step index `null`, and a missing key is not a mismatch at all. Neither may be turned into a
    // raise by the fix above.
    assert_eq!(run(".a", "null\n").0, "null\n");
    assert_eq!(run(".z", "a: 1\n").0, "null\n");
}

#[test]
fn the_optional_suffix_still_suppresses_the_mismatch() {
    // The raise is a real raise, so `?` and a collect both see it as one.
    assert_eq!(run(".a?", "- 1\n- 2\n- 3\n").0, "");
    assert_eq!(run("[.a?]", "- 1\n- 2\n- 3\n").0, "[]\n");
}
