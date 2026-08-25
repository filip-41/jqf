//! The canonical-identity echo route, end-to-end: identity over a single CANONICAL JSON document publishes the retained
//! source verbatim, and any shape whose render is not the source declines to the re-encoding rungs.
//!
//! The route died silently once: the render plan's coefficient range straddles the fraction point for every nonzero
//! integer part, so `decimal_source_is_canonical` declined on `[1.5]` while `[0.5]` — whose first non-zero digit sits
//! past the point — fired, and the echo was live only on zero-integer-part inputs. These probes pin the matrix BOTH
//! ways: every self-render spelling must fire `roundtrip` with byte-identical echo, and every disqualifier (exponent,
//! non-minimal escape, duplicate key, nine-key object) must decline with the floor's bytes.

use std::process::{Command, Output, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// Runs jqf with stdin redirected from a regular file (the seekable shape the whole-document rungs require) and returns
/// the output.
fn run_file(args: &[&str], input: &[u8]) -> Output {
    let path = std::env::temp_dir().join(format!(
        "jqf-echo-route-{}-{}",
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

/// The `--explain` route line of one run.
fn route_of(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr
        .lines()
        .find_map(|line| line.strip_prefix("jqf: explain: route: "))
        .unwrap_or_else(|| panic!("no route line in: {stderr}"))
        .to_owned()
}

/// Runs `program` over `input` and returns (route, stdout).
fn run(program: &str, input: &[u8]) -> (String, Vec<u8>) {
    let output = run_file(&["--no-parallel", "--explain", "-c", program], input);
    assert_eq!(
        output.status.code(),
        Some(0),
        "program {program} over {input:?} exited {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    (route_of(&output), output.stdout)
}

#[test]
fn self_render_decimal_spellings_fire_the_echo() {
    // The straddle family: every spelling whose integer part is nonzero names a coefficient range that crosses the
    // point (`1.5` names `1.5`), and every all-zero magnitude (`0.0`, `-0.0`) names the empty range. All of them are
    // their own compact render, so all of them must be served by the echo route and echoed byte for byte — the facade
    // newline included.
    let canonical: [(&str, &[u8]); 12] = [
        (".", b"[0.5]"),
        (".", b"[1.5]"),
        (".", b"[10.5]"),
        (".", b"[1.05]"),
        (".", b"[-1.5]"),
        (".", b"[-0.5]"),
        (".", b"[0.0]"),
        (".", b"[-0.0]"),
        (".", b"[100.0]"),
        (".", b"[0.100]"),
        (".", b"{\"a\":1.5}"),
        (".", b"{\"a\":1,\"b\":[2.5,3.0]}"),
    ];
    for (program, input) in canonical {
        let (route, stdout) = run(program, input);
        assert_eq!(
            route, "roundtrip",
            "program {program} over {input:?} must take the echo route"
        );
        let mut expected = input.to_vec();
        expected.push(b'\n');
        assert_eq!(stdout, expected, "the echo must be the retained source verbatim");
    }
}

#[test]
fn a_trailing_newline_still_fires_the_echo() {
    // The trailing line feed is the facade's suffix, never part of the echoed value bytes: a canonical document WITH a
    // trailing newline still echoes.
    let (route, stdout) = run(".", b"[1.5]\n");
    assert_eq!(route, "roundtrip");
    assert_eq!(stdout, b"[1.5]\n");
}

#[test]
fn echo_bytes_match_the_forced_floor() {
    // The decline law's obligation: whatever the echo serves, its bytes are the floor's bytes. Every canonical spelling
    // above is compared against the forced whole-document floor (`[.][0] | (P)`), whose route can never be the echo.
    let documents: [&[u8]; 8] = [
        b"[0.5]",
        b"[1.5]",
        b"[10.5]",
        b"[1.05]",
        b"[-1.5]",
        b"[-0.5]",
        b"[0.0]",
        b"{\"a\":1.5}",
    ];
    for document in &documents {
        let (route, echoed) = run(".", document);
        assert_eq!(route, "roundtrip", "over {document:?}");
        let (_floor_route, floor_bytes) = run("[.][0] | .", document);
        assert_eq!(echoed, floor_bytes, "echo bytes differ from the floor for {document:?}");
    }
}

#[test]
fn disqualifiers_decline_to_the_floor() {
    // The disqualifier side of the matrix: exponent numbers (`1e3` renders `1000`), non-minimal escapes (`\/`, `\u`),
    // duplicate keys, and nine-key objects all clear the canonicality verdict and must NOT be echoed — the re-encoding
    // rungs own their bytes.
    let non_canonical: [&[u8]; 7] = [
        b"[1e3]",
        b"[1.5e3]",
        b"[\"a\\/b\"]",
        b"[\"\\u0041\"]",
        b"{\"a\":1,\"a\":2}",
        b"{\"a0\":0,\"a1\":1,\"a2\":2,\"a3\":3,\"a4\":4,\"a5\":5,\"a6\":6,\"a7\":7,\"a8\":8}",
        b"{\"a\": [1e2, 2]}",
    ];
    for document in &non_canonical {
        let (route, rendered) = run(".", document);
        assert_ne!(
            route, "roundtrip",
            "a non-self-render source must decline the echo, got {route} for {document:?}"
        );
        let (_floor_route, floor_bytes) = run("[.][0] | .", document);
        assert_eq!(
            rendered, floor_bytes,
            "the fallback bytes differ from the floor for {document:?}"
        );
    }
}

#[test]
fn the_echo_declines_under_byte_rewriting_flags() {
    // The byte-identity echo cannot honor a formatting flag that REWRITES bytes: `-S` reorders members and `-a`
    // re-escapes characters, so the source is no longer the answer. Both must decline to the re-encoding rungs, which
    // apply the flags through the encoder. `-r` (and `-j`/`--raw-output0`, which are `-r` plus a terminator) belongs in
    // the same family: it re-spells a root string without its quotes, so `-rc` printed the quoted source where `-r`
    // alone printed the raw text — the same request answered two ways by an unrelated flag.
    for flag in ["-S", "-a", "-r", "-j", "--raw-output0"] {
        let output = run_file(
            &["--no-parallel", "--explain", flag, "-c", "."],
            b"{\"a\":[1.5],\"b\":[2]}",
        );
        assert_eq!(output.status.code(), Some(0), "flag {flag}");
        assert_ne!(route_of(&output), "roundtrip", "the echo must decline under {flag}");
    }
    // The pretty render is not the source either: default (pretty) output declines the echo.
    let output = run_file(&["--no-parallel", "--explain", "."], b"{\"a\":[1.5]}");
    assert_eq!(output.status.code(), Some(0));
    assert_ne!(route_of(&output), "roundtrip");
}
