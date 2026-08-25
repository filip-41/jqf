//! Record inputs REFUSING markup output (`--output-format xml|html`). Companion to
//! `record_yaml_output.rs`.
//!
//! Pins:
//!
//! 1. A record input never encodes markup: both formats are refused with a usage error naming the
//!    supported set, raised before a byte is written.
//! 2. The refusal holds on every route shape that reaches the record output selection — a regular
//!    FILE (whole-input record route) and a piped (non-seekable) stdin alike.
//!
//! The served cases read a regular FILE (whole-input record route); the last case pins the piped-stdin path.

use std::io::Write as _;
use std::process::{Command, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// Writes `body` to a temp file named by `tag` and returns its path.
fn temp_records(tag: &str, body: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("jqf-record-markup-{}-{tag}.ndjson", std::process::id()));
    std::fs::write(&path, body).expect("temp records written");
    path
}

/// Runs `jqf args…` with `stdin` as the input, returning (exit code, stdout, stderr).
fn run(args: &[&str], stdin: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let mut child = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("jqf spawns");
    child
        .stdin
        .as_mut()
        .expect("stdin piped")
        .write_all(stdin.as_bytes())
        .expect("stdin written");
    let output = child.wait_with_output().expect("jqf waits");
    (output.status.code().expect("exit code"), output.stdout, output.stderr)
}

const RECORDS: &str = "{\"name\":\"alpha\",\"n\":1}\n{\"name\":\"beta\",\"n\":2}\n";
const REFUSAL_XML: &str = "--output-format xml is not supported on a record input; record routes encode json, ndjson, json-seq, csv, tsv, render, or yaml";
const REFUSAL_HTML: &str = "--output-format html is not supported on a record input; record routes encode json, ndjson, json-seq, csv, tsv, render, or yaml";

#[test]
fn xml_output_on_a_record_input_is_a_usage_refusal() {
    let path = temp_records("xml", RECORDS);
    let path_str = path.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(
        &["--input-format", "ndjson", "--output-format", "xml", ".", &path_str],
        "",
    );
    let _ = std::fs::remove_file(&path);
    assert_eq!(code, 2, "stderr: {}", String::from_utf8_lossy(&stderr));
    assert!(
        stdout.is_empty(),
        "nothing may publish before the refusal: {}",
        String::from_utf8_lossy(&stdout)
    );
    assert!(
        String::from_utf8_lossy(&stderr).contains(REFUSAL_XML),
        "{}",
        String::from_utf8_lossy(&stderr)
    );
}

#[test]
fn html_output_on_a_record_input_is_a_usage_refusal() {
    let path = temp_records("html", RECORDS);
    let path_str = path.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(
        &["--input-format", "ndjson", "--output-format", "html", ".", &path_str],
        "",
    );
    let _ = std::fs::remove_file(&path);
    assert_eq!(code, 2, "stderr: {}", String::from_utf8_lossy(&stderr));
    assert!(
        stdout.is_empty(),
        "nothing may publish before the refusal: {}",
        String::from_utf8_lossy(&stdout)
    );
    assert!(
        String::from_utf8_lossy(&stderr).contains(REFUSAL_HTML),
        "{}",
        String::from_utf8_lossy(&stderr)
    );
}

#[test]
fn the_markup_refusal_holds_on_the_piped_route_too() {
    // The piped incremental route shares the record output selection, so the same request through a
    // non-seekable stdin is refused identically rather than served or silently re-encoded.
    let (code, stdout, stderr) = run(&["--input-format", "ndjson", "--output-format", "xml", "."], RECORDS);
    assert_eq!(code, 2, "stderr: {}", String::from_utf8_lossy(&stderr));
    assert!(stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&stderr).contains(REFUSAL_XML),
        "{}",
        String::from_utf8_lossy(&stderr)
    );
}
