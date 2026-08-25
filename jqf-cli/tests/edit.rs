//! Integration coverage for the edit lane CLI surface:
//!
//! - `--edit` publishes the whole EDITED document per input document (exactly
//!   one program output per document; zero/multiple outputs error);
//! - output destination is orthogonal: stdout, `--output PATH`, or
//!   `--in-place` (a flag since 058 W2: the positional input files are the edit targets, each edited independently;
//!   atomic by default, `--no-atomic` opts out);
//! - jq-style positional `FILE` arguments select the input;
//! - `--roundtrip` is removed; edit + NDJSON output is a usage error.

use std::io::Write as _;
use std::process::{Command, Output, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

fn run_jqf(args: &[&str], input: &[u8]) -> Output {
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
    if let Err(error) = child.stdin.take().expect("stdin is piped").write_all(input) {
        assert!(
            error.kind() == std::io::ErrorKind::BrokenPipe,
            "input writes to jqf's stdin: {error}"
        );
    }
    child.wait_with_output().expect("jqf runs to completion")
}

fn temp_path(name: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("jqf-edit-{name}-{}", std::process::id()));
    path
}

#[test]
fn edit_replaces_only_the_touched_span() {
    let output = run_jqf(&["-c", "--edit", ".b = [3]"], b"{\"a\":1,\"b\":[1,2]}");
    assert!(output.status.success(), "edit succeeds: {output:?}");
    assert_eq!(output.stdout, b"{\"a\":1,\"b\":[3]}");
    assert!(output.stderr.is_empty());
}

#[test]
fn flat_config_edit_splices_and_keeps_the_comments_and_spacing() {
    // The S2 gate: a leaf edit patches ONE span on a flat config file — comments, separator spacing, and key order
    // survive; a structural append copies the sibling's separator and spacing; the capability is declared by the codec
    // (the 039 drift-class flip).
    let output = run_jqf(
        &["--edit", "--input-format", "ini", ".db.port = 5432"],
        b"# db\n[db]\nhost = localhost\n",
    );
    assert!(output.status.success(), "ini edit succeeds: {output:?}");
    assert_eq!(output.stdout, b"# db\n[db]\nhost = localhost\nport = 5432\n");

    let output = run_jqf(
        &["--edit", "--input-format", "properties", ".name = \"bob\""],
        b"# top\nname = ada\nid=42\n",
    );
    assert!(output.status.success(), "properties edit: {output:?}");
    assert_eq!(output.stdout, b"# top\nname = bob\nid=42\n");
}

#[test]
fn edit_echoes_the_whole_document_for_a_reader_program() {
    // The output subject is the document, so `.a` publishes the unchanged document byte-identical to the input, not the
    // value `1`.
    let output = run_jqf(&["--edit", ".a"], b"{ \"a\" : 1 }");
    assert!(output.status.success(), "edit succeeds: {output:?}");
    assert_eq!(output.stdout, b"{ \"a\" : 1 }");
    assert!(output.stderr.is_empty());
}

#[test]
fn multi_output_edit_errors_without_output() {
    let output = run_jqf(&["--edit", ".a = (1,2)"], b"{}");
    assert!(!output.status.success(), "multi-output edit must fail: {output:?}");
    assert!(output.stdout.is_empty(), "a failing edit publishes nothing");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("jqf:"), "the failure must be reported, got: {stderr:?}");
    assert!(
        !stderr.contains("unknown option"),
        "the flag must EXIST; the failure must be the edit lane's, got: {stderr:?}"
    );
}

#[test]
fn zero_output_edit_errors_without_output() {
    let output = run_jqf(&["--edit", "empty"], b"{}");
    assert!(!output.status.success(), "zero-output edit must fail: {output:?}");
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("unknown option"),
        "the flag must EXIST; the failure must be the edit lane's, got: {stderr:?}"
    );
}

#[test]
fn adjacent_documents_are_each_edited() {
    let output = run_jqf(&["-c", "--edit", ".a = 9"], b"{\"a\":1}\n{\"a\":2}\n");
    assert!(output.status.success(), "per-document edits succeed: {output:?}");
    assert_eq!(output.stdout, b"{\"a\":9}\n{\"a\":9}\n");
    assert!(output.stderr.is_empty());
}

///  `--edit --check` is the gofmt -l verdict — exit 0 when
/// the would-be output is byte-identical, exit 1 when it differs, and NOTHING printed and NOTHING written in either
/// case.
#[test]
fn edit_check_exits_zero_when_the_edit_would_not_change_the_file() {
    let path = temp_path("check-unchanged");
    std::fs::write(&path, b"{\"a\": 1}\n").expect("test input writes");
    let output = run_jqf(&["--edit", "--check", ".", path.to_str().expect("path is UTF-8")], b"");
    assert_eq!(output.status.code(), Some(0), "an unchanged edit exits 0: {output:?}");
    assert!(output.stdout.is_empty(), "check prints nothing: {output:?}");
    assert!(output.stderr.is_empty(), "check prints nothing: {output:?}");
    // The file is untouched on disk.
    assert_eq!(std::fs::read(&path).expect("file still readable"), b"{\"a\": 1}\n");
    let _ = std::fs::remove_file(&path);
}

/// Identity `--edit` on a file with no trailing newline is byte-identical to the input; `--check` agrees (exit 0)
/// instead of treating the facade newline as a change.
#[test]
fn identity_edit_of_a_newline_less_file_is_byte_identical() {
    let input = b"{\"a\":1}";
    let output = run_jqf(&["--edit", "."], input);
    assert!(output.status.success(), "identity edit succeeds: {output:?}");
    assert_eq!(
        output.stdout, input,
        "identity --edit does not append a newline the input never had"
    );
    assert!(output.stderr.is_empty());

    let path = temp_path("check-newline-less");
    std::fs::write(&path, input).expect("test input writes");
    let check = run_jqf(&["--edit", "--check", ".", path.to_str().expect("path is UTF-8")], b"");
    assert_eq!(
        check.status.code(),
        Some(0),
        "identity --check on a newline-less file exits 0: {check:?}"
    );
    assert!(check.stdout.is_empty(), "check prints nothing: {check:?}");
    assert!(check.stderr.is_empty(), "check prints nothing: {check:?}");
    assert_eq!(
        std::fs::read(&path).expect("file still readable"),
        input,
        "check writes nothing"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn edit_check_exits_one_when_the_edit_would_change_the_file() {
    let path = temp_path("check-changed");
    std::fs::write(&path, b"{\"a\": 1}\n").expect("test input writes");
    let output = run_jqf(
        &["--edit", "--check", ".a = 2", path.to_str().expect("path is UTF-8")],
        b"",
    );
    assert_eq!(output.status.code(), Some(1), "a changing edit exits 1: {output:?}");
    assert!(output.stdout.is_empty(), "check prints nothing: {output:?}");
    assert!(output.stderr.is_empty(), "check prints nothing: {output:?}");
    // The change was never written.
    assert_eq!(std::fs::read(&path).expect("file still readable"), b"{\"a\": 1}\n");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn check_without_edit_is_a_usage_error() {
    let output = run_jqf(&["--check", "."], b"{\"a\":1}\n");
    assert_eq!(
        output.status.code(),
        Some(2),
        "bare --check is a usage error: {output:?}"
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("--check may only be used with --edit"), "{stderr}");
}

#[test]
fn positional_file_is_read_as_input() {
    let path = temp_path("input");
    std::fs::write(&path, b"{\"a\":1}").expect("test input writes");
    let output = run_jqf(&["-c", "--edit", ".b = 2", path.to_str().expect("path is UTF-8")], b"");
    let _ = std::fs::remove_file(&path);
    assert!(output.status.success(), "positional file edit succeeds: {output:?}");
    assert_eq!(output.stdout, b"{\"a\":1,\"b\":2}");
    assert!(output.stderr.is_empty());
}

#[test]
fn output_path_writes_the_document() {
    let path = temp_path("output");
    let output = run_jqf(
        &[
            "-c",
            "--edit",
            ".b = 2",
            "--output",
            path.to_str().expect("path is UTF-8"),
        ],
        b"{\"a\":1}",
    );
    assert!(output.status.success(), "output-path edit succeeds: {output:?}");
    assert!(output.stdout.is_empty(), "no stdout when a file is written");
    let written = std::fs::read(&path).expect("output file exists");
    let _ = std::fs::remove_file(&path);
    assert_eq!(written, b"{\"a\":1,\"b\":2}");
    assert!(output.stderr.is_empty());
}

#[test]
fn in_place_edits_the_file_and_preserves_its_trailing_bytes() {
    let path = temp_path("inplace");
    std::fs::write(&path, b"{\"a\":1}").expect("test input writes");
    let output = run_jqf(
        &[
            "-c",
            "--edit",
            "--in-place",
            ".b = 2",
            path.to_str().expect("path is UTF-8"),
        ],
        b"",
    );
    assert!(output.status.success(), "in-place edit succeeds: {output:?}");
    let written = std::fs::read(&path).expect("file still exists");
    let _ = std::fs::remove_file(&path);
    // The source had no trailing newline, and in-place preservation keeps it that way: the file is the patched document
    // with its original tail.
    assert_eq!(written, b"{\"a\":1,\"b\":2}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

///  the atomic-replace model is honest — a hardlinked target
/// edited atomically DETACHES (the sibling keeps the old inode's content), while `--no-atomic` writes the original
/// inode so the sibling sees the edit.
#[test]
fn atomic_edit_detaches_hardlinks_and_no_atomic_keeps_them() {
    // Pair 1: the atomic edit. The write renames a NEW inode over the target, so the sibling (still on the old inode)
    // keeps its content.
    let target = temp_path("hardlink-target");
    let sibling = temp_path("hardlink-sibling");
    std::fs::write(&target, b"{\"a\":1}\n").expect("target writes");
    std::fs::hard_link(&target, &sibling).expect("hard link");
    let atomic = run_jqf(
        &[
            "--edit",
            "--in-place",
            ".a = 2",
            target.to_str().expect("path is UTF-8"),
        ],
        b"",
    );
    assert!(atomic.status.success(), "atomic edit: {atomic:?}");
    assert_eq!(
        std::fs::read(&target).expect("target"),
        b"{\"a\":2}\n",
        "the target carries the edit"
    );
    assert_eq!(
        std::fs::read(&sibling).expect("sibling"),
        b"{\"a\":1}\n",
        "the hardlink sibling keeps the old inode's content (documented \
         atomic-replace model)"
    );
    let _ = std::fs::remove_file(&target);
    let _ = std::fs::remove_file(&sibling);

    // Pair 2: the --no-atomic edit writes the ORIGINAL inode directly, so the sibling sees the change.
    let target = temp_path("noatomic-target");
    let sibling = temp_path("noatomic-sibling");
    std::fs::write(&target, b"{\"a\":1}\n").expect("target writes");
    std::fs::hard_link(&target, &sibling).expect("hard link");
    let no_atomic = run_jqf(
        &[
            "--edit",
            "--in-place",
            "--no-atomic",
            ".a = 3",
            target.to_str().expect("path is UTF-8"),
        ],
        b"",
    );
    assert!(no_atomic.status.success(), "no-atomic edit: {no_atomic:?}");
    assert_eq!(
        std::fs::read(&sibling).expect("sibling after no-atomic"),
        b"{\"a\":3}\n",
        "--no-atomic edits the original inode, so the sibling sees the change"
    );
    let _ = std::fs::remove_file(&target);
    let _ = std::fs::remove_file(&sibling);
}

#[test]
fn no_atomic_is_accepted_for_in_place() {
    let path = temp_path("noatomic");
    std::fs::write(&path, b"{\"a\":1}").expect("test input writes");
    let output = run_jqf(
        &[
            "-c",
            "--edit",
            "--no-atomic",
            "--in-place",
            ".b = 2",
            path.to_str().expect("path is UTF-8"),
        ],
        b"",
    );
    assert!(output.status.success(), "non-atomic in-place edit succeeds: {output:?}");
    let written = std::fs::read(&path).expect("file still exists");
    let _ = std::fs::remove_file(&path);
    assert_eq!(written, b"{\"a\":1,\"b\":2}");
}

#[test]
fn roundtrip_flag_is_removed() {
    let output = run_jqf(&["--roundtrip", "."], b"{}");
    assert!(
        !output.status.success(),
        "the removed flag must be rejected: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unknown option"),
        "the rejection must name the unknown option, got: {:?}",
        output.stderr
    );
}

#[test]
fn edit_with_ndjson_output_is_a_usage_error() {
    let output = run_jqf(&["--edit", "--output-format", "ndjson", "."], b"{}");
    assert!(
        !output.status.success(),
        "edit + NDJSON output must be refused: {output:?}"
    );
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("unknown option"),
        "the flag must EXIST; the refusal must be the usage check's, got: {stderr:?}"
    );
}

#[test]
fn materialized_numbers_keep_their_authored_spelling() {
    // Ledger item 13: `1.000` must survive decode -> materialize -> encode as `1.000`, exactly as jq preserves an
    // untouched number's literal form.
    let output = run_jqf(&["."], b"1.000");
    assert!(output.status.success(), "identity on 1.000: {output:?}");
    assert_eq!(output.stdout, b"1.000\n");
}

#[test]
fn edit_reports_an_uncaught_raised_error_to_stderr() {
    // 037 finding 6: the edit lane's buffered sink dropped every runtime diagnostic (exit 5, empty stdout AND stderr).
    // The non-edit route reports `jqf: error (at <stdin>:1): boom` for the same program; the edit lane must match it
    // byte-for-byte, not just carry the exit code.
    let output = run_jqf(&["--edit", ".a = error(\"boom\")"], b"{ \"a\" : 1 }\n");
    assert!(!output.status.success(), "a raised error must fail: {output:?}");
    assert_eq!(output.status.code(), Some(5), "runtime-error exit class");
    assert!(output.stdout.is_empty(), "a failing edit publishes nothing: {output:?}");
    assert_eq!(
        output.stderr,
        b"jqf: error (at <stdin>:1): boom\n".to_vec(),
        "the diagnostic must not go missing: {output:?}"
    );
}

#[test]
fn edit_reports_an_arithmetic_runtime_error_to_stderr() {
    let output = run_jqf(&["--edit", ".a = (1/0)"], b"{ \"a\" : 1 }\n");
    assert!(!output.status.success(), "division by zero must fail: {output:?}");
    assert_eq!(output.status.code(), Some(5), "runtime-error exit class");
    assert!(output.stdout.is_empty());
    assert!(
        !output.stderr.is_empty(),
        "the diagnostic must not go missing: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("jqf:"), "the frame must be present, got: {stderr:?}");
}

#[test]
fn in_place_edits_each_file_independently() {
    // 058 W2 ruling: `--in-place` edits every positional file INDEPENDENTLY — one run per file, that file's bytes read
    // and its output written back to itself — so each file keeps its own trailing bytes.
    let path_a = temp_path("multi-a");
    let path_b = temp_path("multi-b");
    std::fs::write(&path_a, b"{\"a\":1}").expect("test input writes");
    std::fs::write(&path_b, b"{\"a\":2}\n").expect("test input writes");
    let output = run_jqf(
        &[
            "-c",
            "--edit",
            "--in-place",
            ".a = 9",
            path_a.to_str().expect("path is UTF-8"),
            path_b.to_str().expect("path is UTF-8"),
        ],
        b"",
    );
    let written_a = std::fs::read(&path_a).expect("file a exists");
    let written_b = std::fs::read(&path_b).expect("file b exists");
    let _ = std::fs::remove_file(&path_a);
    let _ = std::fs::remove_file(&path_b);
    assert!(
        output.status.success(),
        "independent in-place edits succeed: {output:?}"
    );
    assert!(output.stdout.is_empty(), "in-place publishes to the files");
    assert_eq!(written_a, b"{\"a\":9}", "file a edited, trailing bytes kept");
    assert_eq!(
        written_b, b"{\"a\":9}\n",
        "file b edited independently, its own trailing newline kept"
    );
}

#[test]
fn in_place_with_slurp_is_a_usage_error() {
    // 058 W2 ruling: `-s` slurps every file into ONE array, so there is no coherent file to write the single result
    // back to; the combination is refused rather than silently overwriting every file with one run's bytes.
    let path = temp_path("inplace-slurp");
    std::fs::write(&path, b"{\"a\":1}").expect("test input writes");
    let output = run_jqf(&["--in-place", "-s", ".", path.to_str().expect("path is UTF-8")], b"");
    assert!(!output.status.success(), "-s + --in-place must be refused");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--in-place"),
        "the refusal must name --in-place, got: {:?}",
        output.stderr
    );
}

#[test]
fn in_place_with_null_input_is_a_usage_error() {
    // `-n` runs the filter once over null; editing a file with a null-run has no coherent subject, so the combination
    // is refused.
    let path = temp_path("inplace-null");
    std::fs::write(&path, b"{\"a\":1}").expect("test input writes");
    let output = run_jqf(&["--in-place", "-n", ".", path.to_str().expect("path is UTF-8")], b"");
    assert!(!output.status.success(), "-n + --in-place must be refused");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--in-place"),
        "the refusal must name --in-place, got: {:?}",
        output.stderr
    );
}

#[test]
fn in_place_requires_a_positional_file() {
    let output = run_jqf(&["--in-place", "."], b"");
    assert!(!output.status.success(), "--in-place with no file must be refused");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--in-place"),
        "the refusal must name --in-place, got: {:?}",
        output.stderr
    );
}

#[test]
fn in_place_aborts_on_the_failing_file_and_leaves_later_files_untouched() {
    // A failing file aborts the whole request: earlier files stand (already committed), the failing file is untouched,
    // and later files are never edited.
    let path_a = temp_path("abort-a");
    let path_b = temp_path("abort-b");
    let path_c = temp_path("abort-c");
    std::fs::write(&path_a, b"{\"a\":1}\n").expect("test input writes");
    std::fs::write(&path_b, b"{\"a\":2}\n").expect("test input writes");
    std::fs::write(&path_c, b"{\"a\":3}\n").expect("test input writes");
    let output = run_jqf(
        &[
            "-c",
            "--edit",
            "--in-place",
            ".a = error(\"boom\")",
            path_a.to_str().expect("path is UTF-8"),
            path_b.to_str().expect("path is UTF-8"),
            path_c.to_str().expect("path is UTF-8"),
        ],
        b"",
    );
    let written_a = std::fs::read(&path_a).expect("file a exists");
    let written_b = std::fs::read(&path_b).expect("file b exists");
    let written_c = std::fs::read(&path_c).expect("file c exists");
    let _ = std::fs::remove_file(&path_a);
    let _ = std::fs::remove_file(&path_b);
    let _ = std::fs::remove_file(&path_c);
    assert_eq!(output.status.code(), Some(5), "runtime-error exit class");
    assert_eq!(written_a, b"{\"a\":1}\n", "the failing file is untouched");
    assert_eq!(
        written_b, b"{\"a\":2}\n",
        "a later file is never edited after a failure"
    );
    assert_eq!(
        written_c, b"{\"a\":3}\n",
        "the final file is never reached after a failure"
    );
}

#[test]
fn in_place_reports_a_runtime_error_and_leaves_the_file_intact() {
    // 037 confirmed the file safety already holds (a failing in-place edit never touches the original bytes); this pins
    // the reporting half of the same failure, over the SAME `edit` route --in-place shares with `--edit`.
    let path = temp_path("inplace-error");
    std::fs::write(&path, b"{ \"a\" : 1 }\n").expect("test input writes");
    let output = run_jqf(
        &[
            "--edit",
            "--in-place",
            ".a = error(\"boom\")",
            path.to_str().expect("path is UTF-8"),
        ],
        b"",
    );
    let written = std::fs::read(&path).expect("file still exists");
    let _ = std::fs::remove_file(&path);
    assert!(!output.status.success(), "a raised error must fail: {output:?}");
    assert_eq!(output.status.code(), Some(5), "runtime-error exit class");
    assert_eq!(
        written, b"{ \"a\" : 1 }\n",
        "a failing in-place edit must leave the original file byte-intact"
    );
    assert_eq!(
        output.stderr,
        b"jqf: error (at <stdin>:1): boom\n".to_vec(),
        "the diagnostic must not go missing: {output:?}"
    );
}

#[test]
fn a_pushed_down_mismatch_reports_to_stderr() {
    // The edit drive's pushed-down mismatch is a typed failure the facade classifies as "already streamed to stderr";
    // before the fix the drive never streamed it, so `--edit '.b'` over an array exited 5 with NO stderr line —
    // indistinguishable from a crash. jq prints `Cannot index array with string` for the same program, so the line must
    // appear here too, and the exit class stays 5.
    let output = run_jqf(&["--edit", ".b"], b"[1,2]");
    assert_eq!(output.status.code(), Some(5), "runtime-error exit class");
    assert_eq!(output.stdout, b"", "a failing edit publishes nothing");
    assert_eq!(
        output.stderr,
        b"jqf: error (at <stdin>:0): Cannot index array with string (\"b\")\n".to_vec(),
        "the pushed-down mismatch must not go missing: {output:?}"
    );
}

#[test]
fn in_place_and_output_conflict_is_a_usage_error() {
    let path_b = temp_path("conflict-b");
    let output = run_jqf(
        &[
            "--edit",
            "--in-place",
            "--output",
            path_b.to_str().expect("path is UTF-8"),
            ".",
        ],
        b"",
    );
    assert!(!output.status.success(), "two destinations must be refused: {output:?}");
    assert!(!path_b.exists(), "nothing may be written on a usage error");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("unknown option"),
        "the flag must EXIST; the refusal must be the usage check's, got: {stderr:?}"
    );
}

// ---------------------------------------------------------------------------
//: `--edit` widened to TOML and YAML. The CLI gate now requires
// matching JSON/TOML/YAML formats (the lane never changes format), and pinning an input format defaults the output to
// it.

#[test]
fn toml_edit_patches_the_span_and_keeps_comments() {
    // The TOML gate: ONE hunk, comment byte-identical.
    let output = run_jqf(
        &["--edit", "--input-format", "toml", ".port = 9090"],
        b"# server\nport = 8080\nname = \"old\"\n",
    );
    assert!(output.status.success(), "toml edit succeeds: {output:?}");
    assert_eq!(output.stdout, b"# server\nport = 9090\nname = \"old\"\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn toml_edit_defaults_the_output_format_to_the_input_format() {
    // `--input-format toml` alone must not produce JSON output: the edit lane defaults the output format to the
    // input's.
    let output = run_jqf(&["--edit", "--input-format", "toml", ".port = 9090"], b"port = 8080\n");
    assert!(output.status.success(), "toml edit succeeds: {output:?}");
    assert_eq!(output.stdout, b"port = 9090\n");
}

#[test]
fn yaml_edit_patches_the_span_and_keeps_comments() {
    // The YAML gate: ONE hunk, comment byte-identical.
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", ".port = 9090"],
        b"# server\nport: 8080\nname: old\n",
    );
    assert!(output.status.success(), "yaml edit succeeds: {output:?}");
    assert_eq!(output.stdout, b"# server\nport: 9090\nname: old\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn yaml_edit_keeps_comments_and_spelling_in_a_float_bearing_document() {
    // A span-less float or bool elsewhere in the document must not knock the edit lane off the patch path: an unchanged
    // float/bool leaf is judged UNCHANGED semantically, so the comment, the authored `1.50` spelling, and the file's
    // trailing newline all survive verbatim. Pre-fix, the whole document silently re-encoded: comment dropped, `1.50`
    // became `1.5`, the trailing newline vanished.
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", ".port = 9090"],
        b"# top\nname: app\nport: 8080\nnested:\n  x: 1.50\n  flag: true\n",
    );
    assert!(output.status.success(), "yaml edit succeeds: {output:?}");
    assert_eq!(
        output.stdout,
        b"# top\nname: app\nport: 9090\nnested:\n  x: 1.50\n  flag: true\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn toml_edit_keeps_comments_and_spelling_in_a_float_bearing_document() {
    let output = run_jqf(
        &["--edit", "--input-format", "toml", ".port = 9090"],
        b"# comment\nport = 8080\nratio = 1.50\n",
    );
    assert!(output.status.success(), "toml edit succeeds: {output:?}");
    assert_eq!(output.stdout, b"# comment\nport = 9090\nratio = 1.50\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn json_edit_keeps_compact_layout_in_a_decimal_bearing_document() {
    // The same law on JSON: an unchanged decimal leaf keeps its authored spelling AND the document's compact layout.
    // Pre-fix, the decimal leaf's missing span forced a whole-document re-encode that pretty-printed the document.
    let output = run_jqf(&["--edit", ".y = 3"], b"{\"x\": 1.0, \"y\": 2}\n");
    assert!(output.status.success(), "json edit succeeds: {output:?}");
    assert_eq!(output.stdout, b"{\"x\": 1.0, \"y\": 3}\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn yaml_edit_of_a_float_leaf_keeps_the_comments() {
    // The Level-2 residual: editing a span-less leaf ITSELF (a float) used to re-encode the whole document from the
    // program's output value, dropping every comment. The authored span is now recorded out-of-band, so the changed
    // leaf patches its span and the surrounding comments survive.
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", ".nested.x = 2.5"],
        b"# top\nname: app\nnested:\n  # deep comment\n  x: 1.50\n",
    );
    assert!(output.status.success(), "float-leaf edit succeeds: {output:?}");
    assert_eq!(
        output.stdout,
        b"# top\nname: app\nnested:\n  # deep comment\n  x: 2.5\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn yaml_edit_of_a_bool_leaf_keeps_the_comments() {
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", ".flag = false"],
        b"# c\nflag: true\nport: 8080\n",
    );
    assert!(output.status.success(), "bool-leaf edit succeeds: {output:?}");
    assert_eq!(output.stdout, b"# c\nflag: false\nport: 8080\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn toml_edit_of_a_float_leaf_keeps_the_comments() {
    let output = run_jqf(
        &["--edit", "--input-format", "toml", ".ratio = 2.5"],
        b"# comment\nratio = 1.50\nport = 8080\n",
    );
    assert!(output.status.success(), "toml float-leaf edit succeeds: {output:?}");
    assert_eq!(output.stdout, b"# comment\nratio = 2.5\nport = 8080\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn yaml_edit_of_a_float_leaf_preserves_an_unchanged_floats_spelling() {
    // The authored span must not break an UNCHANGED float's spelling: the semantic unchanged test keeps `1.50` verbatim
    // while a sibling float is patched.
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", ".nested.y = 3.0"],
        b"nested:\n  x: 1.50\n  y: 2.0\n",
    );
    assert!(output.status.success(), "sibling-float edit succeeds: {output:?}");
    assert_eq!(output.stdout, b"nested:\n  x: 1.50\n  y: 3.0\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn yaml_structural_edit_restores_the_trailing_newline() {
    // A structural change (a new key) re-encodes the whole document; the re-encode must not drop the file's own final
    // newline.
    let output = run_jqf(&["--edit", "--input-format", "yaml", ".x = 1"], b"port: 8080\n");
    assert!(output.status.success(), "yaml edit succeeds: {output:?}");
    assert_eq!(output.stdout, b"port: 8080\nx: 1\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn yaml_structural_edit_of_a_newline_less_source_stays_newline_less() {
    let output = run_jqf(&["--edit", "--input-format", "yaml", ".x = 1"], b"port: 8080");
    assert!(output.status.success(), "yaml edit succeeds: {output:?}");
    assert_eq!(output.stdout, b"port: 8080\nx: 1");
    assert!(output.stderr.is_empty());
}

// --------------------------------------------------------------------------- The edit lane's two silent-re-render
// holes: a deletion had no splice policy at all, and a growth splice never carried its siblings' leaf patches. Both
// fell to the whole-document floor, which drops every comment and respells every authored number — silently, at any
// strictness.

#[test]
fn yaml_edit_deleting_a_member_keeps_the_comments() {
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", "del(.db.host)"],
        b"# top\nname: app\ndb:\n  host: x\n  pool: 1.50\n",
    );
    assert!(output.status.success(), "yaml delete succeeds: {output:?}");
    assert_eq!(output.stdout, b"# top\nname: app\ndb:\n  pool: 1.50\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn yaml_edit_deleting_a_member_keeps_that_member_s_own_comment_lines() {
    // The removed member owns the comment lines attached above it: cutting the statement without them would strand a
    // comment over the next key.
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", "del(.b)"],
        b"a: 1\n# about b\nb: 2\nc: 3\n",
    );
    assert!(output.status.success(), "yaml delete succeeds: {output:?}");
    assert_eq!(output.stdout, b"a: 1\nc: 3\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn yaml_edit_deleting_the_last_member_keeps_the_comments() {
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", "del(.c)"],
        b"# top\na: 1.50\nb: 2\nc: 3\n",
    );
    assert!(output.status.success(), "yaml delete succeeds: {output:?}");
    assert_eq!(output.stdout, b"# top\na: 1.50\nb: 2\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn yaml_edit_deleting_a_sequence_item_keeps_the_comments() {
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", "del(.list[1])"],
        b"# top\nlist:\n  - a\n  - b\n  - c\nport: 1.50\n",
    );
    assert!(output.status.success(), "yaml delete succeeds: {output:?}");
    assert_eq!(output.stdout, b"# top\nlist:\n  - a\n  - c\nport: 1.50\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn toml_edit_deleting_a_member_keeps_the_other_members_comments() {
    // A deleted member takes ITS OWN comment lines with it — the same ownership `.name |.@comment` reports — and leaves
    // every other member's bytes, comments, and authored spelling alone.
    let output = run_jqf(
        &["--edit", "--input-format", "toml", "del(.name)"],
        b"# about name\nname = \"app\"\n# about ratio\nratio = 1.50\n",
    );
    assert!(output.status.success(), "toml delete succeeds: {output:?}");
    assert_eq!(output.stdout, b"# about ratio\nratio = 1.50\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn toml_edit_deleting_a_quoted_string_member_cuts_its_closing_quote() {
    // A quoted scalar's span is its INNER content, so the cut has to step over the closing quote to see that the line
    // ends there.
    let output = run_jqf(
        &["--edit", "--input-format", "toml", "del(.name)"],
        b"# cfg\nport = 8080\nname = \"app\"\n",
    );
    assert!(output.status.success(), "toml delete succeeds: {output:?}");
    assert_eq!(output.stdout, b"# cfg\nport = 8080\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn yaml_edit_growth_plus_a_sibling_change_keeps_the_comments() {
    // Two statements: one grows the root, one changes a nested leaf. The growth splice alone does not render the leaf
    // change, so pre-fix the re-decode verification failed and the whole document re-encoded.
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", ".db.host = \"y\" | .extra = 1"],
        b"# top\nname: app\ndb:\n  host: x\n  pool: 1.50\n",
    );
    assert!(output.status.success(), "yaml edit succeeds: {output:?}");
    assert_eq!(
        output.stdout,
        b"# top\nname: app\ndb:\n  host: \"y\"\n  pool: 1.50\nextra: 1\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn toml_edit_growth_plus_a_sibling_change_keeps_the_comments() {
    let output = run_jqf(
        &["--edit", "--input-format", "toml", ".db.pool = 5 | .newtop = \"x\""],
        b"# cfg\nname = \"app\"\nport = 8080\n\n[db]\nhost = \"x\"\n",
    );
    assert!(output.status.success(), "toml edit succeeds: {output:?}");
    assert_eq!(
        output.stdout,
        b"# cfg\nname = \"app\"\nport = 8080\nnewtop = \"x\"\n\n[db]\nhost = \"x\"\npool = 5\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn yaml_edit_in_place_keeps_the_comment() {
    let path = temp_path("yaml-inplace");
    std::fs::write(&path, b"# server\nport: 8080\n").expect("test input writes");
    let output = run_jqf(
        &[
            "--edit",
            "--in-place",
            "--input-format",
            "yaml",
            ".port = 9090",
            path.to_str().expect("path is UTF-8"),
        ],
        b"",
    );
    assert!(output.status.success(), "in-place yaml edit succeeds: {output:?}");
    let written = std::fs::read(&path).expect("file still exists");
    let _ = std::fs::remove_file(&path);
    assert_eq!(written, b"# server\nport: 9090\n");
}

#[test]
fn comment_round_trip_keeps_further_spaces_and_tabs() {
    // 144 §8's first round-trip case: read `PATH.@comment` and feed the value straight back as `PATH.@comment =
    // <value>`; the output must be byte-identical to the input. 139 DRIFT1 (b) / 144 S0-T1 make that true for a comment
    // whose text carries further spaces or a tab: today `# two spaces` reads `"two spaces"` (trim) and writes back `#
    // two spaces`.
    for (format, fixture) in [
        ("yaml", b"#  two spaces\nk: 1\n".as_slice()),
        ("toml", b"#  two spaces\nk = 1\n".as_slice()),
    ] {
        let output = run_jqf(
            &["--edit", "--input-format", format, ".k.@comment = (.k.@comment)"],
            fixture,
        );
        assert_eq!(
            output.status.code(),
            Some(0),
            "{format} comment round-trip compiles: {output:?}"
        );
        assert_eq!(
            &output.stdout, fixture,
            "{format} read-then-write-back is byte-identical"
        );
    }
}

#[test]
fn cross_format_edit_is_refused() {
    let output = run_jqf(
        &[
            "--edit",
            "--input-format",
            "yaml",
            "--output-format",
            "toml",
            ".port = 1",
        ],
        b"port: 8080\n",
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "cross-format edit is a usage error: {output:?}"
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn json_fact_write_is_a_compile_rejection() {
    // The B1 law: a fact write splices comment lines into the retained source, and JSON cannot carry comments. The
    // write must be rejected at argument time naming the contract — NOT by the input decode, which would blame the
    // user's (fine) JSON file.
    let output = run_jqf(&["--edit", ".key.@comment = [\"x\"]"], b"{\"key\":1}");
    assert_eq!(
        output.status.code(),
        Some(2),
        "a JSON fact write is a usage error: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot carry this fact write"),
        "the refusal names the contract: {stderr}"
    );
    assert!(
        stderr.contains("fact-carrying format"),
        "the refusal points at the way out: {stderr}"
    );
    // The 2026-08-18 pin excluded jsonc/json5 here because their comment write then exited 5 (: the splice landed
    // inside the string literal); the fix re-enabled both, so the refusal names them again — and every format it names
    // must actually serve the write (each has its own round-trip test in this file).
    assert!(
        stderr.contains("jsonc") && stderr.contains("json5"),
        "the refusal names the JSON-family escapes whose comment write lands: {stderr}"
    );
    assert!(!stderr.contains("jqfjson"), "the removed profile stays gone: {stderr}");
    assert!(output.stdout.is_empty());
}

#[test]
fn json_attribute_fact_write_refusal_names_the_class_not_comments() {
    // The gate fires on ANY fact write, not just comments — `.&href` and `.@tag` are FactAssign too. The refusal must
    // name the fact-write class, never hard-code the comment spelling for a shape the user did not write.
    let output = run_jqf(&["--edit", ".a.&href = \"u\""], b"{\"a\":1}");
    assert_eq!(
        output.status.code(),
        Some(2),
        "a JSON attribute write is a usage error: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot carry this fact write"),
        "the refusal names the class: {stderr}"
    );
    assert!(
        !stderr.contains("@comment") && !stderr.contains("comments"),
        "the refusal must not misname an attribute write as comments: {stderr}"
    );
}

#[test]
fn jsonc_comment_write_inserts_a_slash_comment() {
    // `.a.@comment = ["note"]` is the supported write. JSONC emits `//` immediately before the member so the decode
    // attaches it to `.a`.
    let output = run_jqf(
        &["--input-format", "jsonc", "--edit", ".a.@comment = [\"note\"]"],
        b"{\"a\":1}",
    );
    assert!(output.status.success(), "jsonc comment write must succeed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("// note"),
        "jsonc write must emit a // comment: {stdout}"
    );
    assert!(stdout.contains("\"a\""), "jsonc write must keep the member: {stdout}");
}

#[test]
fn json5_comment_write_inserts_a_slash_comment() {
    let output = run_jqf(
        &["--input-format", "json5", "--edit", ".a.@comment = [\"note\"]"],
        b"{\"a\":1}",
    );
    assert!(output.status.success(), "json5 comment write must succeed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("// note"),
        "json5 write must emit a // comment: {stdout}"
    );
    assert!(stdout.contains("\"a\""), "json5 write must keep the member: {stdout}");
}

#[test]
fn jsonc_comment_write_on_a_file_keeps_the_comment() {
    let path = temp_path("comment.jsonc");
    std::fs::write(&path, b"{\"a\":1}").expect("write fixture");
    let output = run_jqf(
        &[
            "--input-format",
            "jsonc",
            "--edit",
            ".a.@comment = [\"note\"]",
            path.to_str().expect("utf-8 temp path"),
        ],
        b"",
    );
    let _ = std::fs::remove_file(&path);
    assert!(
        output.status.success(),
        "jsonc file comment write must succeed: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("// note"),
        "jsonc file write must emit a // comment: {stdout}"
    );
}

#[test]
fn jsonc_comment_write_on_a_string_member_splices_before_the_member() {
    // The A2 defect : a string node's retained span records its INNER content (the bound-source convention — no
    // quotes), so the comment-write splice treated the first inner byte as the token start, failed its key walk-back,
    // and inserted the `//` line INSIDE the string literal. The patched buffer then failed the verification re-decode
    // (`json.control-in-string`) and the edit exited 5 with the file untouched — on exactly the tsconfig-shaped
    // documents (comments, trailing comma, string members) the format exists for.
    let input = b"{\n  // the user name\n  \"name\": \"alice\",\n  /* role */\n  \"role\": \"admin\",\n}\n";
    let output = run_jqf(
        &["--input-format", "jsonc", "--edit", ".name.@comment = [\"# hello\"]"],
        input,
    );
    assert!(
        output.status.success(),
        "jsonc comment write on a string member must succeed: {output:?} {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The comment replaces the block above the member; every byte outside the edited span survives verbatim.
    assert_eq!(
        output.stdout,
        b"{\n  // # hello\n  \"name\": \"alice\",\n  /* role */\n  \"role\": \"admin\",\n}\n"
    );
}

#[test]
fn json_fact_write_is_refused_even_with_an_explicit_json_format() {
    // Same law with the format spelled out: the explicit `--input-format json` must not change the lane — JSON still
    // cannot carry comments.
    let output = run_jqf(
        &["--edit", "--input-format", "json", ".key.@comment = [\"x\"]"],
        b"{\"key\":1}",
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "a JSON fact write is a usage error even with the format explicit: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot carry this fact write"),
        "the refusal names the contract: {stderr}"
    );
}

#[test]
fn non_binding_format_edit_is_refused() {
    // HTML has no edit tier (ruled edit out permanently), so the edit lane refuses it up front rather than silently
    // whole-re-encoding. (CBOR joined the edit lane with its T5 splice — E7 — so it is no longer the refusal canary
    // this test once was.)
    let output = run_jqf(
        &["--edit", "--input-format", "html", "--output-format", "html", "."],
        b"",
    );
    assert_eq!(output.status.code(), Some(2), "html edit is refused: {output:?}");
}

#[test]
fn alias_edit_refuses_by_default_and_the_flag_is_the_escape() {
    // The law, pinned at the CLI boundary: an edit whose diff touches an alias-referenced YAML node refuses with prose
    // (exit 5, zero stdout). The escape (`--edit-expand-alias`) lifts the refusal and completes with the anchor-rewrite
    // semantics: exit 0, the anchor/alias structure flattened to the program's value, and a warning naming the alias on
    // stderr. The flag is an ACCEPTANCE of the rewrite the refusal describes, never a correctness fix.
    let fixture = b"a: &x 1\nb: *x\n";

    // The default refuses.
    let output = run_jqf(&["--edit", "--input-format", "yaml", ".b = 5"], fixture);
    assert_eq!(
        output.status.code(),
        Some(5),
        "the alias edit refuses by default: {output:?}"
    );
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("alias"), "the refusal names the alias: {stderr}");

    // The flag completes the edit with the anchor-rewrite semantics.
    let output = run_jqf(
        &["--edit", "--edit-expand-alias", "--input-format", "yaml", ".b = 5"],
        fixture,
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "the escape completes the edit: {output:?}"
    );
    assert_eq!(
        output.stdout, b"a: 1\nb: 5\n",
        "the escape flattens the anchor structure to the program's value"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("alias") && stderr.contains("--edit-expand-alias"),
        "the escape warns naming the flag and the alias: {stderr}"
    );

    // The flag is meaningless outside the edit lane: a usage error.
    let output = run_jqf(&["--edit-expand-alias", ".b = 5"], fixture);
    assert_eq!(
        output.status.code(),
        Some(2),
        "the flag without --edit/--in-place is a usage error: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--edit"),
        "the usage refusal names the requirement: {stderr}"
    );
}

// The metadata write channel (145 lane C5, 142 W3 D9/D10/D11 ruled in): `.@style`, `.@tag`, `.@anchor`, `.@alias`
// writes over commented and anchor-bearing YAML fixtures. Each write is a span patch on the retained source, verified
// by re-decode (the value must be unchanged and the written fact must land); a write the codec cannot honor refuses
// with a prose error, never a silent drop (the encode-or-report-a-loss law).

#[test]
fn style_write_requotes_the_scalar_and_round_trips() {
    // `.@style = "double"` re-renders the scalar's span double-quoted.
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", ".foo.@style = \"double\""],
        b"# note\nfoo: bar # trailing\n",
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "a style write compiles and verifies: {output:?}"
    );
    assert_eq!(
        &output.stdout, b"# note\nfoo: \"bar\" # trailing\n",
        "the comment and trailing bytes survive; only the scalar's spelling changes"
    );
    // The re-decoded style fact is the written payload (round trip).
    let read = run_jqf(&["--input-format", "yaml", ".foo.@style"], b"foo: \"bar\"\n");
    assert_eq!(read.stdout, b"\"double\"\n", "{read:?}");
}

#[test]
fn style_write_single_and_literal_round_trip() {
    let single = run_jqf(
        &["--edit", "--input-format", "yaml", ".foo.@style = \"single\""],
        b"foo: bar\n",
    );
    assert_eq!(single.stdout, b"foo: 'bar'\n", "{single:?}");
    let literal = run_jqf(
        &["--edit", "--input-format", "yaml", ".foo.@style = \"literal\""],
        b"foo: bar\n",
    );
    assert_eq!(literal.stdout, b"foo: |-\n  bar\n", "{literal:?}");
    let read = run_jqf(&["--input-format", "yaml", ".foo.@style"], b"foo: |-\n  bar\n");
    assert_eq!(read.stdout, b"\"literal\"\n", "{read:?}");
}

#[test]
fn style_write_plain_that_would_retype_refuses_with_prose() {
    // An EXPLICIT plain request on a text that would re-resolve to another kind refuses (the plain spelling of `true`
    // is a boolean, not the string); the implicit C1 fallback belongs to the leaf splice, not to a user who asked for a
    // style by name.
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", ".foo.@style = \"plain\""],
        b"foo: \"true\"\n",
    );
    assert_eq!(output.status.code(), Some(5), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot be spelled plain"),
        "the refusal names the reason: {stderr}"
    );
}

#[test]
fn style_write_folded_refuses_with_prose() {
    // `folded` is in the authored style vocabulary (decode records it) but the block encoder does not emit folded block
    // scalars: an explicit write refuses loudly rather than silently rendering another style.
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", ".foo.@style = \"folded\""],
        b"foo: bar\n",
    );
    assert_eq!(output.status.code(), Some(5), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot honor the .@style write"),
        "the refusal names the write: {stderr}"
    );
    assert!(output.stdout.is_empty(), "zero bytes published");
}

#[test]
fn style_write_on_a_number_refuses_with_prose() {
    // A style is a STRING-rendering fact; a number has no quoting style that preserves its value (`"1"` would re-decode
    // as a string).
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", ".foo.@style = \"double\""],
        b"foo: 1\n",
    );
    assert_eq!(output.status.code(), Some(5), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("a .@style write applies to a string scalar"),
        "{stderr}"
    );
}

#[test]
fn tag_write_round_trips_a_core_tag() {
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", ".foo.@tag = \"!!str\""],
        b"# note\nfoo: bar\n",
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        &output.stdout, b"# note\nfoo: !!str bar\n",
        "the tag spelling is inserted before the value"
    );
    let read = run_jqf(&["--input-format", "yaml", ".foo.@tag"], b"foo: !!str bar\n");
    assert_eq!(read.stdout, b"\"tag:yaml.org,2002:str\"\n", "{read:?}");
}

#[test]
fn tag_write_round_trips_a_local_tag_on_a_string() {
    // A non-core tag around a string scalar re-types the value: the payload survives, the tag wraps it, and the write's
    // verification strips the tag before the value-identity comparison.
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", ".name.@tag = \"!money\""],
        b"name: gold\n",
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(&output.stdout, b"name: !money gold\n", "{output:?}");
    let read = run_jqf(&["--input-format", "yaml", ".name.@tag"], b"name: !money gold\n");
    assert_eq!(read.stdout, b"\"!money\"\n", "{read:?}");
    // The tagged document survives a floor re-encode (encode-or-report-a-loss).
    let floor = run_jqf(
        &["--input-format", "yaml", "--output-format", "yaml", "."],
        b"name: !money gold\n",
    );
    assert_eq!(floor.stdout, b"name: !money gold\n", "{floor:?}");
}

#[test]
fn tag_write_the_codec_cannot_honor_refuses_with_prose() {
    // A non-core tag around a NON-string scalar is unrepresentable on decode (the scalar's text would not re-resolve as
    // a string under the tag), so the write must refuse loudly, never produce bytes that cannot round-trip.
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", ".foo.@tag = \"!money\""],
        b"foo: 100\n",
    );
    assert_eq!(output.status.code(), Some(5), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("a .@tag write serves a string scalar"),
        "the refusal names the reason: {stderr}"
    );
    assert!(output.stdout.is_empty(), "zero bytes published");
}

#[test]
fn tag_write_core_tag_matching_the_kind_round_trips() {
    // A CORE tag of the node's own kind is honored without re-typing: the value stays exact, the tag lands.
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", ".foo.@tag = \"!!int\""],
        b"foo: 100\n",
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(&output.stdout, b"foo: !!int 100\n", "{output:?}");
    let read = run_jqf(&["--input-format", "yaml", ".foo.@tag"], b"foo: !!int 100\n");
    assert_eq!(read.stdout, b"\"tag:yaml.org,2002:int\"\n", "{read:?}");
}

#[test]
fn tag_write_with_a_malformed_spelling_refuses_with_prose() {
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", ".foo.@tag = \"bad tag\""],
        b"foo: bar\n",
    );
    assert_eq!(output.status.code(), Some(5), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not a valid YAML tag"),
        "the refusal names the spelling: {stderr}"
    );
}

#[test]
fn anchor_write_names_the_node_and_survives_a_floor() {
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", ".foo.@anchor = \"x\""],
        b"# note\nfoo: bar # trailing\n",
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        &output.stdout, b"# note\nfoo: &x bar # trailing\n",
        "the anchor is inserted before the value; comments survive"
    );
    let read = run_jqf(&["--input-format", "yaml", ".foo.@anchor"], b"foo: &x bar\n");
    assert_eq!(read.stdout, b"\"x\"\n", "{read:?}");
    // The anchored document survives a floor re-encode.
    let floor = run_jqf(
        &["--input-format", "yaml", "--output-format", "yaml", "."],
        b"foo: &x bar\n",
    );
    assert_eq!(floor.stdout, b"foo: &x bar\n", "{floor:?}");
}

#[test]
fn alias_write_makes_the_site_an_alias_and_round_trips() {
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", ".b.@alias = \"x\""],
        b"a: &x 1\nb: 1\n",
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(&output.stdout, b"a: &x 1\nb: *x\n", "{output:?}");
    // The aliased document survives a floor re-encode (the C4 emission).
    let floor = run_jqf(
        &["--input-format", "yaml", "--output-format", "yaml", "."],
        b"a: &x 1\nb: *x\n",
    );
    assert_eq!(floor.stdout, b"a: &x 1\nb: *x\n", "{floor:?}");
}

#[test]
fn alias_write_whose_value_would_change_refuses_with_prose() {
    // The anchor's value differs from the node's: the alias would change the document value, which the edit lane's
    // value-identity law forbids.
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", ".b.@alias = \"x\""],
        b"a: &x 1\nb: 2\n",
    );
    assert_eq!(output.status.code(), Some(5), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("the .@alias write would change the value"), "{stderr}");
    assert!(output.stdout.is_empty());
}

#[test]
fn alias_write_naming_an_undefined_anchor_refuses_with_prose() {
    let output = run_jqf(
        &["--edit", "--input-format", "yaml", ".foo.@alias = \"x\""],
        b"foo: bar\n",
    );
    assert_eq!(output.status.code(), Some(5), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("undefined alias"),
        "the re-decode's own prose reaches the user: {stderr}"
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn metadata_writes_over_a_non_yaml_format_are_refused() {
    // TOML carries comments but not the YAML metadata facts: the seam refuses with the role and format named, never
    // wrong bytes.
    let output = run_jqf(
        &["--edit", "--input-format", "toml", ".port.@anchor = \"x\""],
        b"port = 8080\n",
    );
    assert_eq!(output.status.code(), Some(5), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot carry the anchor fact"),
        "the refusal names the format and the role: {stderr}"
    );
}

#[test]
fn edit_output_path_keeps_yaml_comments() {
    let dir = std::env::temp_dir().join(format!(
        "jqf-edit-output-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = std::fs::create_dir_all(&dir);
    let out = dir.join("out.yaml");
    let output = run_jqf(
        &[
            "--edit",
            "--input-format",
            "yaml",
            "--output",
            out.to_str().expect("utf-8"),
            ".port = 9090",
        ],
        b"# server\nport: 8080\nname: old\n",
    );
    assert!(output.status.success(), "edit --output succeeds: {output:?}");
    assert_eq!(
        std::fs::read_to_string(&out).expect("written yaml"),
        "# server\nport: 9090\nname: old\n",
        "path-pinned yaml must still use the edit dialect"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn xml_edit_splices_the_quoted_attribute_bytes() {
    let output = run_jqf(
        &["--edit", "--input-format", "xml", ".&href = \"z\""],
        b"<a href=\"https://x\">y</a>",
    );
    assert!(output.status.success(), "xml attribute edit succeeds: {output:?}");
    assert_eq!(output.stdout, b"<a href=\"z\">y</a>");
    assert!(output.stderr.is_empty());
}

#[test]
fn xml_edit_second_attribute_does_not_depend_on_node_id_adjacency() {
    let output = run_jqf(
        &["--edit", "--input-format", "xml", ".&b = \"2\""],
        b"<a href=\"https://x\" b=\"1\">y</a>",
    );
    assert!(
        output.status.success(),
        "xml second-attribute edit succeeds: {output:?}"
    );
    assert_eq!(output.stdout, b"<a href=\"https://x\" b=\"2\">y</a>");
    assert!(output.stderr.is_empty());
}
