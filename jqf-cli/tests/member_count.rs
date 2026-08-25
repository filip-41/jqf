//! The member-count answers (`length`, `keys | length`) over object and array containers, pinned jq-identical.
//!
//! The structure-count rung that once served these without decoding key text is gone; every spelling now rides the
//! ordinary sequence route, which applies the duplicate-key law (last-value-wins) byte for byte. These tests pin the
//! route (sequence), the jq-identical answers across array/object/scalar containers, and the duplicate-key and
//! non-minimal-escape declines that keep the last-value-wins law exact.

use std::process::{Command, Output, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// Runs jqf with stdin redirected from a regular file.
fn run_file(args: &[&str], input: &[u8]) -> Output {
    let path = std::env::temp_dir().join(format!(
        "jqf-member-count-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, input).expect("input file");
    let output = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(args)
        .stdin(Stdio::from(std::fs::File::open(&path).expect("open input file")))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("jqf runs to completion");
    let _ = std::fs::remove_file(&path);
    output
}

fn route_of(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr
        .lines()
        .find_map(|line| line.strip_prefix("jqf: explain: route: "))
        .unwrap_or_else(|| panic!("no route line in: {stderr}"))
        .to_owned()
}

/// Runs one program through jqf (natural) and jq, asserting the route and the normalized stderr/stdout/exit equality
/// (the `jq:`/`jqf:` prefix on stderr is the binary name, normalized away).
fn assert_served(input: &[u8], program: &str, expected_route: &str) {
    let output = run_file(&["--no-parallel", "--explain", "-c", program], input);
    assert_eq!(output.status.code(), Some(0), "{program}");
    assert_eq!(route_of(&output), expected_route, "{program}");

    let jq = Command::new("jq")
        .args(["-c", program])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child.stdin.take().expect("stdin").write_all(input).expect("write");
            child.wait_with_output()
        })
        .expect("jq runs");
    // The jqf run carries `--explain` (a diagnostic surface that never touches the answered bytes) and the provenance
    // line; strip both from the stderr comparison so the compared stderr is the run's own diagnostics only.
    let normalize = |bytes: &[u8]| {
        String::from_utf8_lossy(bytes)
            .replace("jq:", "TOOL:")
            .replace("jqf:", "TOOL:")
            .lines()
            .filter(|line| !line.starts_with("TOOL: build=") && !line.starts_with("TOOL: explain:"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(normalize(&output.stdout), normalize(&jq.stdout), "stdout for {program}");
    assert_eq!(normalize(&output.stderr), normalize(&jq.stderr), "stderr for {program}");
    assert_eq!(output.status.code(), jq.status.code(), "exit for {program}");
}

#[test]
fn member_count_spellings_answer_over_objects() {
    // `length`, `keys | length`, and `keys_unsorted | length` over an OBJECT container all answer the member count on
    // the sequence route.
    let input = br#"{"alpha":1,"beta":[2,3],"gamma":{"d":4}}"#;
    for program in ["length", "keys | length", "keys_unsorted | length"] {
        assert_served(input, program, "sequence");
    }
}

#[test]
fn the_keys_count_answers_serve_piped_paths() {
    let input = br#"{"outer":{"a":1,"b":2,"c":3}}"#;
    assert_served(input, ".outer | keys | length", "sequence");
    assert_served(input, ".outer | keys_unsorted | length", "sequence");
    assert_served(input, ".outer | length", "sequence");
}

#[test]
fn the_member_count_answers_arrays_exactly_as_before() {
    let input = br"[1,2,3,4,5]";
    for program in ["length", "keys | length", ". [0:3] | length"] {
        assert_served(input, program, "sequence");
    }
}

#[test]
fn duplicate_keys_answer_via_the_floor() {
    // `{"a":1,"a":2}` has ONE member after the last-value-wins law; a raw member count would answer 2. The ordinary
    // route applies the law byte for byte.
    let input = br#"{"a":1,"a":2,"b":3}"#;
    assert_served(input, "length", "sequence");
    assert_served(input, "keys | length", "sequence");
}

#[test]
fn non_minimal_key_escapes_decline_to_the_floor() {
    // `"\u0061"` is the same key value as `"a"` but a different byte spelling: the fingerprint cannot prove them
    // distinct, so the count declines.
    let input = br#"{"a":1,"\u0061":2}"#;
    assert_served(input, "length", "sequence");
    assert_served(input, "keys | length", "sequence");
}

#[test]
fn scalar_containers_still_decline_to_the_floor() {
    // The soundness obligation: codepoint/magnitude semantics never reach the count answer — strings and numbers
    // decline to the floor.
    for input in [br#""hello""#.as_slice(), b"42".as_slice(), b"null".as_slice()] {
        assert_served(input, "length", "sequence");
    }
}
