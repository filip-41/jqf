//! Record inputs publishing YAML output (the `RouteCapability:Record` lift).
//!
//! Pins three laws of the new surface:
//!
//! 1. An NDJSON record stream publishes YAML items with the block profile's
//!    between-documents `---` separator in the right places — one factory serves the whole serial drive, so item 1 opens
//!    bare and every later item opens with the separator (byte-identical to the whole-document ladder's multi-document
//!    YAML output).
//! 2. The drive plans SERIAL for a YAML target (`OutputTarget:is_stateful`),
//!    and `--parallel`'s bytes are identical anyway.
//! 3. The incremental routes refuse the stream-STATEFUL yaml profiles by
//!    name: `--follow`, a piped (non-seekable) stdin, and streaming cycles re-open the encoder factory per cycle, which
//!    would drop the separator at a cycle boundary. A piped stdin hits exactly this refusal even without `--follow` — the
//!    seekability rule routes it through the cycle drive.
//!
//! The served cases read a regular FILE: a redirected file takes the whole-input record route, where one encoder
//! factory serves every item.

use std::io::Write as _;
use std::process::{Command, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// Writes `body` to a temp file named by `tag` and returns its path.
fn temp_records(tag: &str, body: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("jqf-record-yaml-{}-{tag}.ndjson", std::process::id()));
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

const RECORDS: &str = "{\"name\":\"alpha\",\"n\":1}\n{\"name\":\"beta\",\"n\":2}\n{\"name\":\"gamma\",\"n\":3}\n";
// `n` is QUOTED by the core-schema block renderer on purpose: a bare `n` re-resolves as boolean false under YAML 1.1
// readers, so the renderer emits a safe scalar spelling.
const EXPECTED_YAML: &str = "name: alpha\n\"n\": 1\n---\nname: beta\n\"n\": 2\n---\nname: gamma\n\"n\": 3\n";

#[test]
fn ndjson_publishes_yaml_with_between_document_separators() {
    let path = temp_records("main", RECORDS);
    let path_str = path.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(
        &["--input-format", "ndjson", "--output-format", "yaml", ".", &path_str],
        "",
    );
    let _ = std::fs::remove_file(&path);
    assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&stderr));
    // The block profile: item 1 bare, later items open with `---`; the facade owns each item's LF.
    assert_eq!(String::from_utf8(stdout).expect("utf8").as_str(), EXPECTED_YAML);
}

#[test]
fn record_yaml_bytes_equal_the_whole_document_ladder() {
    // The same documents through the whole-document ladder (default JSON adjacent-value input) — the two encoders must
    // agree byte for byte.
    let (code, ladder, stderr) = run(
        &["--output-format", "yaml", "--no-parallel", "."],
        "{\"name\":\"alpha\",\"n\":1}\n{\"name\":\"beta\",\"n\":2}\n{\"name\":\"gamma\",\"n\":3}\n",
    );
    assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&stderr));

    let path = temp_records("ladder", RECORDS);
    let path_str = path.to_string_lossy().into_owned();
    let (code, record, stderr) = run(
        &[
            "--input-format",
            "ndjson",
            "--output-format",
            "yaml",
            "--no-parallel",
            ".",
            &path_str,
        ],
        "",
    );
    let _ = std::fs::remove_file(&path);
    assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&stderr));
    assert_eq!(record, ladder, "record-drive yaml must be byte-identical");
}

#[test]
fn parallel_plans_serial_but_publishes_the_same_bytes() {
    let path = temp_records("parallel", RECORDS);
    let path_str = path.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(
        &[
            "--input-format",
            "ndjson",
            "--output-format",
            "yaml",
            "--parallel",
            ".",
            &path_str,
        ],
        "",
    );
    let _ = std::fs::remove_file(&path);
    assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&stderr));
    assert_eq!(
        String::from_utf8(stdout).expect("utf8").as_str(),
        EXPECTED_YAML,
        "the stateful fence must not change bytes"
    );
}

#[test]
fn follow_refuses_the_stream_stateful_yaml_profiles_by_name() {
    let (code, _stdout, stderr) = run(
        &["--input-format", "ndjson", "--output-format", "yaml", "--follow", "."],
        RECORDS,
    );
    assert_ne!(code, 0);
    let message = String::from_utf8_lossy(&stderr);
    assert!(
        message.contains("stream state"),
        "the refusal must name the mechanism: {message}"
    );
}

#[test]
fn piped_stdin_refuses_block_yaml_naming_the_cycle_law() {
    // No --follow flag: a NON-SEEKABLE stdin with a record format is still the incremental cycle drive (the seekability
    // rule), so the same refusal applies — and names it.
    let (code, _stdout, stderr) = run(&["--input-format", "ndjson", "--output-format", "yaml", "."], RECORDS);
    assert_ne!(code, 0);
    let message = String::from_utf8_lossy(&stderr);
    assert!(
        message.contains("stream state"),
        "the refusal must name the mechanism: {message}"
    );
}
