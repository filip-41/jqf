//! The collect-filter count answers (`[.c[] | select(P)] | length` over the closed predicate vocabulary), pinned
//! jq-identical.
//!
//! The filter row rides the same lazy sequence route as the classic count rows; the document-core consumer answers from
//! the container span's raw bytes and DECLINES — the caller then reruns the whole program, which reproduces the floor
//! byte for byte — on every shape the closed law cannot rank. These tests pin both halves: the answers where the scan
//! engages and the declines (NaN spellings, raising elements, object containers) where the floor must answer.

use std::process::{Command, Output, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// Runs jqf with stdin redirected from a regular file.
fn run_file(args: &[&str], input: &[u8]) -> Output {
    let path = std::env::temp_dir().join(format!(
        "jqf-collect-count-filter-{}-{}",
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

/// Runs one program through jqf (natural) and jq, asserting normalized stderr/stdout/exit equality (the `jq:`/`jqf:`
/// prefix on stderr is the binary name, normalized away).
fn assert_served(input: &[u8], program: &str) {
    let output = run_file(&["--no-parallel", "-c", program], input);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{program}: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

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
    let normalize = |bytes: &[u8]| {
        String::from_utf8_lossy(bytes)
            .replace("jq:", "TOOL:")
            .replace("jqf:", "TOOL:")
            .lines()
            .filter(|line| !line.starts_with("TOOL: build="))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(normalize(&output.stdout), normalize(&jq.stdout), "stdout for {program}");
    assert_eq!(normalize(&output.stderr), normalize(&jq.stderr), "stderr for {program}");
    assert_eq!(output.status.code(), jq.status.code(), "exit for {program}");
}

#[test]
fn comparisons_answer_identically_over_mixed_members() {
    // Positive/negative/zero/absent/null/string members: every per-item law the closed vocabulary owns, in one catalog.
    let input = br#"{"catalog":[
        {"name":"a","stock":5},
        {"name":"b","stock":-1},
        {"name":"c","stock":0},
        {"name":"d"},
        {"name":"e","stock":null},
        {"name":"f","stock":"many"}
    ]}"#;
    assert_served(input, "[.catalog[] | select(.stock > 0)] | length");
    assert_served(input, "[.catalog[] | select(.stock < 1)] | length");
    assert_served(input, "[.catalog[] | select(.stock >= 0)] | length");
    assert_served(input, "[.catalog[] | select(.stock == 0)] | length");
    assert_served(input, "[.catalog[] | select(.stock != 0)] | length");
    assert_served(input, r#"[.catalog[] | select(.stock == "many")] | length"#);
    assert_served(input, r#"[.catalog[] | select(.name == "a")] | length"#);
    assert_served(input, "[.catalog[] | select(.stock)] | length");
}

#[test]
fn duplicate_keys_and_escaped_spellings_follow_the_adopted_law() {
    // Last-value-wins: `{"stock":1,"stock":-5}` is EXCLUDED; an escaped key spelling IS the decoded key.
    let input = br#"{"catalog":[
        {"stock":1,"stock":-5},
        {"st\u0074ock":9},
        {"stock":2}
    ]}"#;
    assert_served(input, "[.catalog[] | select(.stock > 0)] | length");
}

#[test]
fn exact_decimal_laws_survive_exponents() {
    // 1e999 stays exact (true against 0); -0 == 0 (false); tiny exponents.
    let input = br#"{"catalog":[
        {"stock":1e999},
        {"stock":-0},
        {"stock":1e-400},
        {"stock":-12.30e1}
    ]}"#;
    assert_served(input, "[.catalog[] | select(.stock > 0)] | length");
}

#[test]
fn nan_spellings_decline_to_the_floor() {
    // The non-finite spellings are TRUTHY numbers in the adopted process; the scan refuses to guess (`n` prefix shared
    // with null) and declines.
    let input = br#"{"catalog":[{"stock":nan},{"stock":1},{"stock":NaN}]}"#;
    assert_served(input, "[.catalog[] | select(.stock > 0)] | length");
}

#[test]
fn raising_elements_fail_like_the_adopted_law() {
    // `.stock` over a non-object RAISES in both tools: nonzero exit, and identical diagnostics modulo the binary-name
    // prefix.
    let program = "[.catalog[] | select(.stock > 0)] | length";
    let cases = [
        (&br#"{"catalog":[{"stock":1},[2,3]]}"#[..], "array"),
        (&br#"{"catalog":[{"stock":1},"s"]}"#[..], "string"),
        (&br#"{"catalog":[{"stock":1},7]}"#[..], "number"),
    ];
    for (input, kind) in cases {
        let jqf_output = run_file(&["--no-parallel", "-c", program], input);
        assert_ne!(jqf_output.status.code(), Some(0), "{program} must raise");
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
        assert_eq!(jqf_output.status.code(), jq.status.code(), "{input:?}");
        let normalize = |bytes: &[u8]| {
            String::from_utf8_lossy(bytes)
                .replace("jq:", "TOOL:")
                .replace("jqf:", "TOOL:")
        };
        assert_eq!(normalize(&jqf_output.stdout), normalize(&jq.stdout));
        // jqf renders the accessor as `string ("stock")`; jq omits the parens.
        assert_eq!(
            normalize(&jqf_output.stderr),
            format!("TOOL: error (at <stdin>:0): Cannot index {kind} with string (\"stock\")\n")
        );
    }
}

#[test]
fn ranges_count_only_in_range_elements() {
    let input = br#"{"catalog":[{"stock":-1},{"stock":2},{"stock":3},{"stock":4}]}"#;
    assert_served(input, "[.catalog[1:3][] | select(.stock > 0)] | length");
}

#[test]
fn object_containers_decline_to_the_floor() {
    // `.[]` over an OBJECT iterates member values; the v1 span leaf owns arrays only and declines — the floor answers
    // byte for byte.
    let input = br#"{"a":{"x":{"stock":1},"y":{"stock":-1}}}"#;
    assert_served(input, "[.a[] | select(.stock > 0)] | length");
}

#[test]
fn empty_and_all_falsy_catalogs_answer_zero() {
    assert_served(br#"{"catalog":[]}"#, "[.catalog[] | select(.stock > 0)] | length");
    assert_served(
        br#"{"catalog":[{"stock":0},{"stock":null}]}"#,
        "[.catalog[] | select(.stock > 0)] | length",
    );
}

#[test]
fn nested_select_paths_decline_but_stay_correct() {
    // Multi-step predicates are outside the closed vocabulary; the floor answers them exactly as before this row
    // existed.
    let input = br#"{"catalog":[{"m":{"k":5}},{"m":{"k":-5}}]}"#;
    assert_served(input, "[.catalog[] | select(.m.k > 0)] | length");
}
