//! The cross-format encode-projection policy (D1), at the product path.
//!
//! Decode unifies and encode projects: a value the target has no native spelling for is written as canonical text
//! rather than refused, and the run REPORTS what it did. These tests pin the reporting channel and its shape, which no
//! unit test can reach — the warning is emitted once per run by the CLI, after every route has finished, from counters
//! the codecs wrote.
//!
//! The laws pinned here:
//!
//! * warnings go to stderr and NOTHING goes to stdout that was not a value;
//! * one line per projection KIND per run, carrying a COUNT, so a million
//!   projected dates cost one line and not a million;
//! * the event is a PROJECTION, not a source format — a target that spells
//!   the value natively is silent, and a tagged value the program never published is silent;
//! * the projected text is the format's canonical spelling (RFC 3339,
//!   unpadded base64url), byte for byte.
//!
//! `tools/jqf-cli-jq-compat.sh` carries the value rows (`project` kind) but cannot carry these: it discards stderr by
//! design, and jq is no oracle for any of it.

use std::io::Write as _;
use std::process::{Command, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// Runs `jqf args…` with `stdin` as the input, returning (exit code, stdout, stderr).
fn run(args: &[&str], stdin: &str) -> (i32, String, String) {
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
    if let Err(error) = child.stdin.take().expect("stdin is piped").write_all(stdin.as_bytes()) {
        assert!(
            error.kind() == std::io::ErrorKind::BrokenPipe,
            "input writes to jqf's stdin: {error}"
        );
    }
    let output = child.wait_with_output().expect("jqf runs to completion");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stdout).expect("stdout is UTF-8"),
        String::from_utf8(output.stderr).expect("stderr is UTF-8"),
    )
}

#[test]
fn a_published_tag_warns_on_stderr_and_leaves_stdout_pure() {
    // The ledger's own probe case. JSON spells no tag, so the value publishes as its payload — a STRING, because YAML
    // resolution already made it one, and encode does not re-resolve.
    let (code, out, err) = run(&["--input-format", "yaml", "-c", "."], "v: !money 12.5\n");
    assert_eq!(code, 0);
    assert_eq!(out, "{\"v\":\"12.5\"}\n");
    assert_eq!(err, "jqf: warning: 1 tagged value published as its bare payload\n");
}

#[test]
fn one_line_per_kind_carries_a_count_rather_than_repeating() {
    // Three tagged values, one line. The warning is per KIND per RUN: a per-occurrence line would make a large document
    // unreadable, which is the whole reason the counters live on the request rather than at the call.
    let (code, out, err) = run(&["--input-format", "yaml", "-c", "."], "a: !m 1\nb: !n 2\nc: !o 3\n");
    assert_eq!(code, 0);
    assert_eq!(out, "{\"a\":\"1\",\"b\":\"2\",\"c\":\"3\"}\n");
    assert_eq!(err, "jqf: warning: 3 tagged values published as their bare payload\n");
}

#[test]
fn each_projected_kind_gets_its_own_line() {
    // All three kinds in one run, which only CBOR can say: a tag-0 datetime (recognized, so a temporal), an
    // uninterpreted tag, and a byte string. Three different things were done to the data, so there are three lines —
    // the count is per kind, never one lumped total.
    let mut child = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(["--input-format", "cbor", "-c", "."])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("jqf spawns");
    let mut input = vec![0x83, 0xc0, 0x74];
    input.extend_from_slice(b"2026-06-15T12:34:56Z");
    input.extend_from_slice(&[0xd9, 0x04, 0xd2, 0x62, b'h', b'i']);
    input.extend_from_slice(&[0x45, 0x01, 0x02, 0x03, 0x04, 0x05]);
    // A usage-error child exits WITHOUT reading stdin, closing the pipe mid-write; BrokenPipe is the expected race
    // there, not a test failure (surfaced by the 003 linux-amd64 emulated lane, where the child's exit reliably beats
    // the parent's write).
    if let Err(error) = child.stdin.take().expect("stdin is piped").write_all(&input) {
        assert!(
            error.kind() == std::io::ErrorKind::BrokenPipe,
            "input writes to jqf's stdin: {error}"
        );
    }
    let output = child.wait_with_output().expect("jqf runs to completion");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "[\"2026-06-15T12:34:56Z\",\"hi\",\"AQIDBAU\"]\n"
    );
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("1 datetime rendered as an RFC 3339 string"),
        "missing temporal line: {err}"
    );
    assert!(
        err.contains("1 tagged value published as its bare payload"),
        "missing tag line: {err}"
    );
    assert!(
        err.contains("1 byte string rendered as base64url text"),
        "missing bytes line: {err}"
    );
    assert_eq!(err.lines().count(), 3, "unexpected extra output: {err}");
}

#[test]
fn a_target_that_spells_the_value_natively_says_nothing() {
    // TOML has real datetime syntax and YAML has real tags. The event is a PROJECTION, so a run that projected nothing
    // warns about nothing — even though the very same values would warn on the way to JSON.
    let (code, out, err) = run(
        &["--input-format", "toml", "--output-format", "toml", "."],
        "d = 2026-06-15\n",
    );
    assert_eq!(code, 0);
    assert_eq!(out, "d = 2026-06-15\n\n");
    assert_eq!(err, "", "a native spelling must be silent");

    let (code, out, err) = run(
        &[
            "--input-format",
            "yaml",
            "--output-format",
            "yaml",
            "--output-dialect",
            "yaml.single-document@1",
            ".",
        ],
        "v: !money 12.5\n",
    );
    assert_eq!(code, 0);
    assert!(out.contains("!money"), "YAML must keep its own tag: {out}");
    assert_eq!(err, "", "a native tag spelling must be silent");
}

#[test]
fn a_tag_the_program_never_published_is_silent() {
    // The event fires where the projection HAPPENS, so an extraction that reaches past the tag is silent: nothing
    // tagged was ever encoded. This is what makes the warning informative instead of a standing property of the input
    // format.
    let (code, out, err) = run(&["--input-format", "yaml", "-c", ".v.a"], "v: !money\n  a: 1\n");
    assert_eq!(code, 0);
    assert_eq!(out, "1\n");
    assert_eq!(err, "");
}

#[test]
fn every_toml_temporal_spelling_projects_to_canonical_rfc_3339() {
    // One writer produces both the native TOML spelling and the projected text, so the two cannot drift. The fractional
    // second is canonicalized the same way on both paths.
    let (code, out, err) = run(
        &["--input-format", "toml", "-c", "."],
        "d = 2026-06-15\nt = 12:34:56\ndt = 2026-06-15T12:34:56\n\
         odt = 2026-06-15T12:34:56Z\nofs = 2026-06-15T14:34:56+02:00\n",
    );
    assert_eq!(code, 0);
    assert_eq!(
        out,
        "{\"d\":\"2026-06-15\",\"t\":\"12:34:56\",\"dt\":\"2026-06-15T12:34:56\",\
         \"odt\":\"2026-06-15T12:34:56Z\",\"ofs\":\"2026-06-15T14:34:56+02:00\"}\n"
    );
    assert_eq!(
        err,
        "jqf: warning: 5 datetimes rendered as RFC 3339 strings; \
         --types-as-strings reads temporals as their plain text\n"
    );
}

#[test]
fn a_cbor_byte_string_projects_as_unpadded_base64url() {
    // RFC 8949 6.5: base64url (RFC 4648 5), and NO padding. `AQIDBAU` is five bytes, which is a group boundary that a
    // padded encoder would spell `AQIDBAU=`.
    let mut child = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(["--input-format", "cbor", "-c", "."])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("jqf spawns");
    // A usage-error child exits WITHOUT reading stdin, closing the pipe mid-write; BrokenPipe is the expected race
    // there, not a test failure (surfaced by the 003 linux-amd64 emulated lane, where the child's exit reliably beats
    // the parent's write).
    if let Err(error) = child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(&[0x45, 0x01, 0x02, 0x03, 0x04, 0x05])
    {
        assert!(
            error.kind() == std::io::ErrorKind::BrokenPipe,
            "input writes to jqf's stdin: {error}"
        );
    }
    let output = child.wait_with_output().expect("jqf runs to completion");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "\"AQIDBAU\"\n");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "jqf: warning: 1 byte string rendered as base64url text\n"
    );
}

#[test]
fn an_uninterpreted_cbor_tag_publishes_its_payload_through_its_layer() {
    // `d9 04 d2` is CBOR tag 1234, which jqf does not interpret, so the decoder builds jqf-data's kindless tag LAYER
    // around the payload. Every encoder has to descend it: a codec that stops at the layer asks a KINDLESS node for its
    // kind and reports an internal contract violation over a document that is perfectly valid.
    let mut child = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(["--input-format", "cbor", "-c", "[., tag, type]"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("jqf spawns");
    // A usage-error child exits WITHOUT reading stdin, closing the pipe mid-write; BrokenPipe is the expected race
    // there, not a test failure (surfaced by the 003 linux-amd64 emulated lane, where the child's exit reliably beats
    // the parent's write).
    if let Err(error) = child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(&[0xd9, 0x04, 0xd2, 0x62, b'h', b'i'])
    {
        assert!(
            error.kind() == std::io::ErrorKind::BrokenPipe,
            "input writes to jqf's stdin: {error}"
        );
    }
    let output = child.wait_with_output().expect("jqf runs to completion");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "[\"hi\",\"cbor:tag:1234\",\"string\"]\n"
    );
}

#[test]
fn a_target_with_no_canonical_spelling_still_refuses() {
    // The policy is projection, not surrender. TOML has no null and no canonical text for one, so a null is the one
    // thing that still cannot be written — and it fails loudly rather than inventing a spelling.
    let (code, out, err) = run(&["--output-format", "toml", "."], "{\"a\":null}");
    assert_eq!(code, 5, "TOML must still refuse a null: {out} {err}");
    assert!(out.is_empty(), "a refusal must publish nothing: {out}");
}

#[test]
fn a_toml_identity_pipeline_keeps_comments_and_bare_keys() {
    // The config-editing law, TOML side: comments ride `toml.comment@1` facts on the located document and re-emit ahead
    // of their owner; keys the bare-key grammar admits emit BARE, so an untouched file re-emits in the spelling a
    // person wrote, not `"name" = "app"` noise.
    let (code, out, err) = run(
        &["--input-format", "toml", "--output-format", "toml", "."],
        "# cfg\nname = \"app\"\nport = 8080\n\n# database\n[db]\nhost = \"x\"\n",
    );
    assert_eq!(code, 0);
    assert_eq!(err, "");
    assert_eq!(
        out,
        "# cfg\nname = \"app\"\nport = 8080\n\n# database\n[db]\nhost = \"x\"\n\n"
    );
}
