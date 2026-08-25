//! The XML pushed-down routes report the same result the floor does.
//!
//! When the demand analysis pushes a static path into the XML codec, the codec navigates the tree itself and publishes
//! a negative observation instead of a located node. A MISMATCH — a member step with no matching child, or any step
//! past a scalar leaf — must carry the kind of the value it was applied to. Before the 049 fix the routes published a
//! FIXED `String`, so a member step on the document root answered `Cannot index string` where the value model reads
//! array. Since 091 §1 a member step with a MATCHING child NAVIGATES (the b child is selected); an unmatched name stays
//! the hard `Array` mismatch.
//!
//! Each case is therefore asserted against the floor as well as against a literal, by running the same program twice:
//! once as written (pushed down) and once behind `. as $z |`, which forces the whole-document route. The two must agree
//! — that agreement is the property, and a fixed `actual_type` broke it silently.

use std::io::Write as _;
use std::process::{Command, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// Runs `program` over `input` as XML, returning `(stdout, stderr)`.
///
/// `--no-json-facts` keeps the answers at the BARE value model: markup answered as JSON renders its facts by default,
/// and every claim here is about the value the routes navigate, not about how it is presented.
fn run(program: &str, input: &str) -> (String, String) {
    let mut child = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(["-c", "--input-format", "xml", "--no-json-facts", program])
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
fn a_member_step_on_the_document_root_navigates_by_name() {
    // 091 §1: the document element is an array of its children, and a member step navigates them by element name — `.b`
    // selects the b child, the same answer pushed down and at the floor.
    let (pushed_out, pushed_err) = run(".b", "<a><b>1</b></a>");
    let (floor_out, floor_err) = run(". as $z | .b", "<a><b>1</b></a>");
    assert_eq!(
        (pushed_out.as_str(), pushed_err.as_str()),
        (floor_out.as_str(), floor_err.as_str()),
        ".b must answer the same pushed down as it does at the floor"
    );
    assert_eq!(pushed_out, "[\"1\"]\n", "the b child is selected");
    // A deeper path whose FIRST member step matches nothing stays the hard Array mismatch, exactly as the floor does.
    scoped_matches_floor(".a.b", "<a><b>1</b></a>", "Cannot index array with string");
}

#[test]
fn a_member_step_on_a_child_element_names_the_array() {
    // Indexing into the root lands on the `<b>` element, still an array; a member step with no matching child stays the
    // hard Array mismatch.
    scoped_matches_floor(".[0].b", "<a><b>1</b></a>", "Cannot index array with string");
}

#[test]
fn a_step_past_a_scalar_leaf_names_the_string() {
    // `.[0][0]` is the text run "1", a scalar leaf; a step past it is the one mismatch whose kind really is `String`.
    scoped_matches_floor(".[0][0].x", "<a><b>1</b></a>", "Cannot index string with string");
}

#[test]
fn the_legal_negative_observation_still_answers_null() {
    // An out-of-range index is a MISSING path, not a mismatch.
    assert_eq!(run(".[5]", "<a><b>1</b></a>").0, "null\n");
}

#[test]
fn the_optional_suffix_still_suppresses_the_mismatch() {
    // The raise is a real raise, so `?` and a collect both see it as one — now for an UNMATCHED name (a matching name
    // navigates instead).
    assert_eq!(run(".x?", "<a><b>1</b></a>").0, "");
    assert_eq!(run("[.x?]", "<a><b>1</b></a>").0, "[]\n");
    // The matching spelling is NOT suppressed: `.b?` selects the b child.
    assert_eq!(run(".b?", "<a><b>1</b></a>").0, "[\"1\"]\n");
}

#[test]
fn a_plural_member_streams_like_the_floor() {
    let input = "<r><b>1</b><b>2</b></r>";
    let (scoped_out, scoped_err) = run(".b", input);
    let (floor_out, floor_err) = run(". as $d | $d | .b", input);
    assert_eq!(
        (scoped_out.as_str(), scoped_err.as_str()),
        (floor_out.as_str(), floor_err.as_str()),
        ".b must stream the same pushed down as it does at the floor"
    );
    assert_eq!(scoped_out, "[\"1\"]\n[\"2\"]\n");
}

#[test]
fn a_plural_member_index_matches_the_floor() {
    let input = "<r><b>1</b><b>2</b></r>";
    let (scoped_out, scoped_err) = run(".b[0]", input);
    let (floor_out, floor_err) = run(". as $d | $d | .b[0]", input);
    assert_eq!(
        (scoped_out.as_str(), scoped_err.as_str()),
        (floor_out.as_str(), floor_err.as_str()),
        ".b[0] must answer the same pushed down as it does at the floor"
    );
}
