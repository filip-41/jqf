//! Integration coverage for 058 W2 — multiple positional input files.
//!
//! The semantics, :
//!
//! - value streams from every file CONCATENATE in argument order, with NO
//!   separator between files (`1` + `2` is one value `12`; a value can even span a file boundary);
//! - `-s` slurps across ALL files into one array, not per file;
//! - `input_filename` reports the file that produced the CURRENT value (the
//!   file holding its last byte), and `input_line_number` resets per file;
//! - `-n` still opens the files, because `input`/`inputs` read them, but
//!   reports `null`/`0` until something is pulled (jq's pre-read state).

use std::process::{Command, Output, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// Runs jqf over the given files (no stdin), capturing stdout/stderr/status. Returns the output and the full temp paths
/// used, in argument order, so `input_filename` assertions can name the exact path jq-style tools report.
fn run_files(args: &[&str], files: &[(&str, &[u8])]) -> (Output, Vec<std::path::PathBuf>) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static DIR_SEQ: AtomicU32 = AtomicU32::new(0);
    let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("jqf-multifile-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir creates");
    let mut paths = Vec::new();
    for (i, (name, bytes)) in files.iter().enumerate() {
        let path = dir.join(format!("{i}-{name}"));
        std::fs::write(&path, bytes).expect("test input writes");
        paths.push(path);
    }
    let mut command = Command::new(jqf_binary());
    command.env("JQF_NO_CONFIG", "1");
    command.args(args);
    for path in &paths {
        command.arg(path);
    }
    let output = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("jqf spawns")
        .wait_with_output()
        .expect("jqf runs to completion");
    let _ = std::fs::remove_dir_all(&dir);
    (output, paths)
}

/// The JSON-quoted full path string `input_filename` reports for a file.
fn filename_quote(path: &std::path::Path) -> String {
    format!("\"{}\"", path.to_string_lossy())
}

#[test]
fn value_streams_concatenate_in_argument_order() {
    let (output, _) = run_files(
        &["-c", "."],
        &[("a", b"{\"a\":1}\n[1,2]\n"), ("b", b"{\"b\":2}\n\"str\"\n")],
    );
    assert!(output.status.success(), "concat succeeds: {output:?}");
    assert_eq!(output.stdout, b"{\"a\":1}\n[1,2]\n{\"b\":2}\n\"str\"\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn files_join_with_no_separator() {
    // jq's multi-file law: the bytes concatenate with NO separator, so a file ending `1` followed by one starting `2`
    // is the single value `12`.
    let (output, _) = run_files(&["-c", "."], &[("a", b"1"), ("b", b"2")]);
    assert!(output.status.success(), "no-separator join: {output:?}");
    assert_eq!(output.stdout, b"12\n");
}

#[test]
fn a_value_may_span_the_file_boundary() {
    // A value that spans a file boundary is attributed to the file holding its LAST byte.
    let (output, paths) = run_files(&["-c", "input_filename"], &[("a", b"{\"a\": "), ("b", b"1}\n")]);
    assert!(output.status.success(), "split value: {output:?}");
    let expected = format!("{}\n", filename_quote(&paths[1]));
    assert_eq!(output.stdout, expected.as_bytes());
}

#[test]
fn slurp_collects_every_file_into_one_array() {
    let (output, _) = run_files(&["-c", "-s", "."], &[("a", b"1\n2\n"), ("b", b"3\n4\n")]);
    assert!(output.status.success(), "slurp succeeds: {output:?}");
    assert_eq!(output.stdout, b"[1,2,3,4]\n");
}

#[test]
fn input_filename_tracks_the_file_of_each_value() {
    let (output, paths) = run_files(
        &["-c", "input_filename"],
        &[("a.json", b"1\n2\n"), ("b.json", b"3\n4\n")],
    );
    assert!(output.status.success(), "input_filename: {output:?}");
    let expected = format!(
        "{}\n{}\n{}\n{}\n",
        filename_quote(&paths[0]),
        filename_quote(&paths[0]),
        filename_quote(&paths[1]),
        filename_quote(&paths[1])
    );
    assert_eq!(output.stdout, expected.as_bytes());
}

#[test]
fn input_line_number_resets_per_file() {
    let (output, _) = run_files(&["-c", "input_line_number"], &[("a", b"1\n2\n"), ("b", b"3\n4\n")]);
    assert!(output.status.success(), "input_line_number: {output:?}");
    assert_eq!(output.stdout, b"1\n2\n1\n2\n");
}

#[test]
fn a_drain_through_inputs_moves_the_position_too() {
    // `inputs` READS, so the position follows it exactly as it follows `input`: after the drain both report the LAST
    // value pulled, not the driver's. `[inputs] | length, input_line_number, input_filename` over `1\n2\n` + `3\n4\n`
    // answers `3`, then `2` (b's file-local line), then b's path. jqf answered the driver's `1`/a-path until
    // `InputsEmit` learned to mark.
    let (output, paths) = run_files(
        &["-c", "[inputs] | length, input_line_number, input_filename"],
        &[("a.json", b"1\n2\n"), ("b.json", b"3\n4\n")],
    );
    assert!(output.status.success(), "inputs drain: {output:?}");
    let expected = format!("3\n2\n{}\n", filename_quote(&paths[1]));
    assert_eq!(output.stdout, expected.as_bytes());
}

#[test]
fn slurp_reports_the_last_values_position() {
    // After `-s` the shared cursor is parked on the LAST consumed value, so `input_filename` answers the last file and
    // `input_line_number` the last value's file-local line.
    let (output, paths) = run_files(
        &["-c", "-s", "input_filename"],
        &[("a.json", b"1\n2\n"), ("b.json", b"3\n4\n")],
    );
    assert!(output.status.success(), "-s input_filename: {output:?}");
    let expected = format!("{}\n", filename_quote(&paths[1]));
    assert_eq!(output.stdout, expected.as_bytes());
    let (output, _) = run_files(
        &["-c", "-s", "input_line_number"],
        &[("a", b"1\n2\n"), ("b", b"3\n4\n")],
    );
    assert!(output.status.success(), "-s input_line_number: {output:?}");
    assert_eq!(output.stdout, b"2\n");
}

#[test]
fn null_input_opens_files_for_the_input_family() {
    // With `-n` the files are opened only when the program reads the input family; `input_filename`/`input_line_number`
    // report jq's pre-read state (null / 0) until a value is pulled, then the pulled value's file/line.
    let (output, _) = run_files(
        &["-c", "-n", "input_filename"],
        &[("a.json", b"1\n"), ("b.json", b"2\n")],
    );
    assert!(output.status.success(), "-n input_filename: {output:?}");
    assert_eq!(output.stdout, b"null\n");
    let (output, paths) = run_files(
        &["-c", "-n", "input as $x | [input_filename, input_line_number, $x]"],
        &[("a.json", b"1\n"), ("b.json", b"2\n")],
    );
    assert!(output.status.success(), "-n input pull: {output:?}");
    let expected = format!("[{},1,1]\n", filename_quote(&paths[0]));
    assert_eq!(output.stdout, expected.as_bytes());
}

#[test]
fn an_input_pull_marks_the_pulled_value_current() {
    // `input` inside the program marks the PULLED value current, so `input_filename` after an explicit pull names the
    // pulled value's file — the first driver value pulls a value from a, the second driver value pulls one from b (the
    // driver and `input` interleave over the shared stream, exactly as jq's).
    let (output, paths) = run_files(
        &["-c", ". as $x | input as $y | input_filename"],
        &[("a.json", b"1\n2\n"), ("b.json", b"3\n4\n")],
    );
    assert!(output.status.success(), "input marks current: {output:?}");
    let expected = format!("{}\n{}\n", filename_quote(&paths[0]), filename_quote(&paths[1]));
    assert_eq!(output.stdout, expected.as_bytes());
}

#[test]
fn an_empty_file_contributes_no_values() {
    let (output, _) = run_files(&["-c", "."], &[("a", b"1\n"), ("empty", b""), ("b", b"2\n")]);
    assert!(output.status.success(), "empty file mid-list: {output:?}");
    assert_eq!(output.stdout, b"1\n2\n");
}
