//! Integration coverage for the W2 disk ceiling: `--max-spill-disk-bytes` must fail the request with a spill-disk
//! resource error when the cumulative run-file bytes would cross it, leave zero spill residue behind, and — since the
//! ceiling is OPT-IN — impose nothing at all when the flag is absent (the fallback law stays byte-identical).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// One isolated TMPDIR for a spill test, removed on drop. The counter keeps the parallel test threads from colliding on
/// one clock-tick's worth of nanosecond names.
struct SpillDir(PathBuf);

impl SpillDir {
    fn new() -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "jqf-spill-ceiling-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        std::fs::create_dir(&path).expect("spill dir");
        SpillDir(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for SpillDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writes `count` descending numbers (one adjacent JSON text per line), so the sort both reorders them and spills when
/// the key budget is tiny.
fn write_input(path: &Path, count: usize) {
    let mut file = std::fs::File::create(path).expect("input file");
    for i in 0..count {
        writeln!(file, "{}", count - i).expect("input write");
    }
}

/// Any `jqf-spill-*` entry left in `base` — the store directory is the spill-engaged proof AND the residue the
/// ceiling's "zero leftover bytes" claim must rule out.
fn spill_residue(base: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(base)
        .map(|entries| {
            entries
                .filter_map(std::result::Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("jqf-spill-"))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Runs `jqf -s --max-spill-bytes BUDGET [--max-spill-disk-bytes N] 'sort_by([.])'` over the descending input with
/// `base` as TMPDIR.
fn run_spill_sort(input: &Path, base: &Path, budget: u64, disk_ceiling: Option<u64>) -> Output {
    let mut args = vec![
        "-s".to_owned(),
        "-c".to_owned(),
        "--max-spill-bytes".to_owned(),
        budget.to_string(),
    ];
    if let Some(ceiling) = disk_ceiling {
        args.push("--max-spill-disk-bytes".to_owned());
        args.push(ceiling.to_string());
    }
    args.push("sort_by([.])".to_owned());
    let mut child = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("TMPDIR", base)
        .spawn()
        .expect("jqf spawns");
    // A usage-error child exits WITHOUT reading stdin, closing the pipe mid-write; BrokenPipe is the expected race
    // there, not a test failure (surfaced by the 003 linux-amd64 emulated lane, where the child's exit reliably beats
    // the parent's write).
    if let Err(error) = child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(&std::fs::read(input).expect("input reads"))
    {
        assert!(
            error.kind() == std::io::ErrorKind::BrokenPipe,
            "input writes to jqf's stdin: {error}"
        );
    }
    child.wait_with_output().expect("jqf runs to completion")
}

/// The exact sorted array the fixture's descending numbers produce, compact.
fn expected_sorted(count: usize) -> Vec<u8> {
    let mut out = String::from("[");
    for index in 1..=count {
        if index > 1 {
            out.push(',');
        }
        out.push_str(&index.to_string());
    }
    out.push(']');
    out.push('\n');
    out.into_bytes()
}

#[test]
fn filled_disk_ceiling_fails_the_request_with_a_resource_error() {
    let spill = SpillDir::new();
    let input = spill.path().join("input");
    write_input(&input, 300_000);
    // The ceiling admits a few runs (so the store directory EXISTS during the request) and then refuses: the ruling's
    // shape, where "zero leftover bytes" is a real claim about cleanup, not about spill never starting.
    let output = run_spill_sort(&input, spill.path(), 131_072, Some(524_288));
    assert_eq!(
        output.status.code(),
        Some(5),
        "a filled disk ceiling must fail with the runtime class: {output:?}"
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(
        stderr.starts_with("jqf: error: ") && stderr.contains("spill disk limit exceeded"),
        "stderr must report a spill-disk resource-class error, got: {stderr:?}"
    );
    assert!(
        stderr.contains("--max-spill-disk-bytes"),
        "stderr must name the flag that configures the ceiling, got: {stderr:?}"
    );
    for banned in ["SpillDiskBytes", "SpillBytes", "LimitExceeded", "limit_kind", "{", "}"] {
        assert!(
            !stderr.contains(banned),
            "stderr leaks Rust type syntax {banned:?}: {stderr:?}"
        );
    }
    assert!(output.stdout.is_empty(), "no output on a rejected request");
    assert!(
        spill_residue(spill.path()).is_empty(),
        "zero leftover bytes: the spill directory must not survive the refusal"
    );
}

#[test]
fn ceiling_unset_answers_byte_identically_to_a_generous_ceiling() {
    // The fallback law's own claim: the ceiling is opt-in, so with the flag absent the spill path answers exactly as a
    // request whose ceiling admits everything — and both answer correctly. The input is the SAME shape the refusal test
    // above proves spills, so a pass cannot come from the spill path never engaging.
    let spill = SpillDir::new();
    let input = spill.path().join("input");
    write_input(&input, 300_000);
    let unset = run_spill_sort(&input, spill.path(), 131_072, None);
    assert!(unset.status.success(), "no ceiling must impose no ceiling: {unset:?}");
    let generous = run_spill_sort(&input, spill.path(), 131_072, Some(100_000_000));
    assert!(
        generous.status.success(),
        "a generous ceiling must admit the same request: {generous:?}"
    );
    let expected = expected_sorted(300_000);
    assert_eq!(unset.stdout, expected, "the unset run must sort correctly");
    assert_eq!(
        generous.stdout, unset.stdout,
        "the ceiling must never change the published bytes"
    );
    assert!(
        spill_residue(spill.path()).is_empty(),
        "a completed spill must clean up after itself"
    );
}

#[test]
fn duplicate_flag_is_rejected() {
    let output = run_flag_parse(&["--max-spill-disk-bytes", "1024", "--max-spill-disk-bytes", "2048", "."]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("--max-spill-disk-bytes may only be given once"));
}

#[test]
fn missing_flag_value_is_rejected() {
    let output = run_flag_parse(&["--max-spill-disk-bytes"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("--max-spill-disk-bytes requires a value"));
}

#[test]
fn non_numeric_flag_value_is_rejected() {
    let output = run_flag_parse(&["--max-spill-disk-bytes", "not-a-number", "."]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("--max-spill-disk-bytes value is not a valid nonnegative integer"));
}

#[test]
fn disk_ceiling_without_a_spill_budget_is_a_usage_error() {
    let output = run_flag_parse(&["--max-spill-disk-bytes", "4096", "."]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(
        stderr.contains("--max-spill-disk-bytes requires --max-spill-bytes"),
        "{stderr}"
    );
}

fn run_flag_parse(args: &[&str]) -> Output {
    Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("jqf runs to completion")
}
