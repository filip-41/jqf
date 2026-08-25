//! The `--split-exp` destination (§S2, the D18–D20 rulings): one file per published ITEM, its path the expression's
//! single string output over that item, with `$index` bound to the item counter.
//!
//! This file pins what the rulings name: the per-item file receipts (N items → N files with the expected names and
//! bytes), the `$index` counter (D18), the destination exclusions in the two-destinations wording (D19), the
//! missing-parent-directory refusal that never `mkdir -p`s a program-derived path (D20), and the non-string split
//! result refusal naming the item index and the produced kind. The `$index` conflict with a user `--arg index` binding
//! is refused at parse time (D18's never-a-silent-shadow law).

use std::io::{Read as _, Write as _};
use std::path::Path;
use std::process::{Command, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// Runs `jqf args…` with `stdin` as the input from `cwd`, returning (exit code, stdout, stderr).
fn run_in(cwd: &Path, args: &[&str], stdin: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let mut command = Command::new(jqf_binary());
    command.env("JQF_NO_CONFIG", "1");
    command.current_dir(cwd);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("jqf spawns");
    if let Err(error) = child.stdin.take().expect("stdin is piped").write_all(stdin.as_bytes()) {
        assert!(
            error.kind() == std::io::ErrorKind::BrokenPipe,
            "stdin write failed: {error}"
        );
    }
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child
        .stdout
        .take()
        .expect("stdout is piped")
        .read_to_end(&mut stdout)
        .expect("read stdout");
    child
        .stderr
        .take()
        .expect("stderr is piped")
        .read_to_end(&mut stderr)
        .expect("read stderr");
    let status = child.wait().expect("jqf exits");
    (status.code().unwrap_or(-1), stdout, stderr)
}

/// A fresh temp directory as the working directory (so split outputs land in a known, clean tree), removed on drop.
struct WorkDir(std::path::PathBuf);

impl WorkDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("jqf-split-exp-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create work dir");
        // The split expressions under test target `out/…`; the directory exists so the D20 missing-parent refusal is
        // exercised by its OWN case (which names `nodir/`), never by these.
        std::fs::create_dir_all(path.join("out")).expect("create out dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// N adjacent JSON values → N files, each named by the expression over the item, each carrying exactly that item's
/// encoded bytes.
#[test]
fn one_file_per_published_item() {
    let work = WorkDir::new("items");
    let (code, stdout, stderr) = run_in(
        work.path(),
        &["--split-exp", "\"out/\" + .name + \".json\"", "."],
        "{\"name\":\"a\",\"v\":1}\n{\"name\":\"b\",\"v\":2}\n{\"name\":\"c\",\"v\":3}\n",
    );
    assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&stderr));
    assert!(stdout.is_empty(), "split publishes no stdout: {stdout:?}");
    for (name, expected) in [
        ("a.json", "{\n  \"name\": \"a\",\n  \"v\": 1\n}\n"),
        ("b.json", "{\n  \"name\": \"b\",\n  \"v\": 2\n}\n"),
        ("c.json", "{\n  \"name\": \"c\",\n  \"v\": 3\n}\n"),
    ] {
        let bytes = std::fs::read(work.path().join("out").join(name))
            .unwrap_or_else(|error| panic!("missing split file {name}: {error}"));
        assert_eq!(String::from_utf8_lossy(&bytes), expected, "file {name}");
    }
}

/// The same receipts over the NDJSON record route (`--input-format ndjson`).
#[test]
fn one_file_per_record() {
    let work = WorkDir::new("records");
    let (code, stdout, stderr) = run_in(
        work.path(),
        &[
            "--input-format",
            "ndjson",
            "--split-exp",
            "\"out/\" + .name + \".json\"",
            ".",
        ],
        "{\"name\":\"a\"}\n{\"name\":\"b\"}\n",
    );
    assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&stderr));
    assert!(stdout.is_empty());
    assert!(work.path().join("out/a.json").is_file());
    assert!(work.path().join("out/b.json").is_file());
}

/// D18: `$index` is the item counter, 0-based, in yq's spelling.
#[test]
fn the_index_counter_binds_per_item() {
    let work = WorkDir::new("index");
    let (code, _stdout, stderr) = run_in(
        work.path(),
        &["--split-exp", "\"out/\\($index).json\"", "."],
        "{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n",
    );
    assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&stderr));
    for (name, expected) in [
        ("0.json", "{\n  \"a\": 1\n}\n"),
        ("1.json", "{\n  \"a\": 2\n}\n"),
        ("2.json", "{\n  \"a\": 3\n}\n"),
    ] {
        let bytes = std::fs::read(work.path().join("out").join(name))
            .unwrap_or_else(|error| panic!("missing split file {name}: {error}"));
        assert_eq!(String::from_utf8_lossy(&bytes), expected, "file {name}");
    }
}

/// `--split-exp-file` reads the expression from a file, `-f`'s shape.
#[test]
fn the_expression_can_come_from_a_file() {
    let work = WorkDir::new("file");
    std::fs::write(work.path().join("expr.txt"), "\"out/\" + .name + \".json\"").expect("write expression file");
    let (code, _stdout, stderr) = run_in(
        work.path(),
        &["--split-exp-file", "expr.txt", "."],
        "{\"name\":\"q\"}\n",
    );
    assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&stderr));
    let bytes = std::fs::read(work.path().join("out/q.json")).expect("the file-named expression produced out/q.json");
    assert_eq!(String::from_utf8_lossy(&bytes), "{\n  \"name\": \"q\"\n}\n");
}

/// A split result that is not a single string is refused naming the item index and the produced kind (the ruling's
/// non-string law).
#[test]
fn a_non_string_split_result_is_refused() {
    let work = WorkDir::new("nonstring");
    let (code, _, stderr) = run_in(work.path(), &["--split-exp", "\"out/\" + .v", "."], "{\"v\":1}\n");
    assert_eq!(code, 2, "stderr: {}", String::from_utf8_lossy(&stderr));
    let text = String::from_utf8_lossy(&stderr);
    assert!(
        text.contains("item 0") && text.contains("number"),
        "the refusal names the item index and the kind: {text}"
    );
}

/// D19: the split destination is mutually exclusive with the two existing destinations and with the document-subject
/// lanes, in the two-destinations wording.
#[test]
fn the_destination_exclusions_are_refused() {
    let work = WorkDir::new("exclusions");
    let mut input_file = work.path().to_path_buf();
    input_file.push("f.json");
    std::fs::write(&input_file, "{\"a\":1}\n").expect("write input file");
    for (args, needle) in [
        (
            vec!["--split-exp", "\"./x.json\"", "--output", "out.json", "."],
            "--split-exp and --output are two destinations",
        ),
        (
            vec!["--split-exp", "\"./x.json\"", "--in-place", "."],
            "--split-exp and --in-place are two destinations",
        ),
        (
            vec!["--split-exp", "\"./x.json\"", "--edit", "."],
            "--split-exp cannot be combined with --edit",
        ),
        (
            vec!["--split-exp", "\"./x.json\"", "--diff", "a", "b", "."],
            "--split-exp cannot be combined with --diff",
        ),
    ] {
        let (code, _, stderr) = run_in(work.path(), &args, "{\"a\":1}\n");
        assert_eq!(code, 2, "args {args:?}: {}", String::from_utf8_lossy(&stderr));
        let text = String::from_utf8_lossy(&stderr);
        assert!(text.contains(needle), "args {args:?}: expected {needle:?} in {text}");
    }
}

/// D18: a user binding named `index` conflicts with the `$index` the split expression binds — refused, never silently
/// shadowed either way.
#[test]
fn a_user_index_binding_is_refused() {
    let work = WorkDir::new("index-binding");
    for args in [
        vec!["--split-exp", "\"out/\\($index).json\"", "--arg", "index", "5", "."],
        vec!["--split-exp", "\"out/\\($index).json\"", "--argjson", "index", "5", "."],
        vec![
            "--split-exp",
            "\"out/\\($index).json\"",
            "--rawfile",
            "index",
            "f.json",
            ".",
        ],
    ] {
        let (code, _, stderr) = run_in(work.path(), &args, "{\"a\":1}\n");
        assert_eq!(code, 2, "args {args:?}: {}", String::from_utf8_lossy(&stderr));
        let text = String::from_utf8_lossy(&stderr);
        assert!(text.contains("--split-exp binds $index"), "args {args:?}: {text}");
    }
}

/// D20: a missing parent directory is an error naming the path — jqf never `mkdir -p`s a path a program derived from
/// untrusted input.
#[test]
fn a_missing_parent_directory_is_refused_naming_the_path() {
    let work = WorkDir::new("missing-dir");
    let (code, _, stderr) = run_in(
        work.path(),
        &["--split-exp", "\"nodir/\" + .name + \".json\"", "."],
        "{\"name\":\"a\"}\n",
    );
    assert_eq!(code, 2, "stderr: {}", String::from_utf8_lossy(&stderr));
    let text = String::from_utf8_lossy(&stderr);
    assert!(
        text.contains("nodir/a.json") && !work.path().join("nodir").exists(),
        "the refusal names the path and creates nothing: {text}"
    );
}

/// `--no-atomic` is honored on the split destination (the same terms as `--output`): the file exists after the run with
/// the item's bytes.
#[test]
fn no_atomic_is_honored() {
    let work = WorkDir::new("no-atomic");
    let (code, _, stderr) = run_in(
        work.path(),
        &["--split-exp", "\"out/\" + .name + \".json\"", "--no-atomic", "."],
        "{\"name\":\"z\"}\n",
    );
    assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&stderr));
    let bytes = std::fs::read(work.path().join("out/z.json")).expect("the non-atomic split file exists");
    assert_eq!(String::from_utf8_lossy(&bytes), "{\n  \"name\": \"z\"\n}\n");
}

/// `-e` still judges the last output value's truthiness: the exit-status facts ride the item reports, which the split
/// sink updates exactly as the ordinary sink does.
#[test]
fn exit_status_rides_the_split_items() {
    let work = WorkDir::new("exit-status");
    let (code, _, stderr) = run_in(
        work.path(),
        &["--split-exp", "\"out/\\($index).json\"", "-e", "."],
        "false\n",
    );
    assert_eq!(
        code,
        1,
        "a false last value exits 1: {}",
        String::from_utf8_lossy(&stderr)
    );
}
