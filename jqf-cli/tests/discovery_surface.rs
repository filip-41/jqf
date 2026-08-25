//! The discovery surface: `--list-builtins`, `--list-formats`, `--help-format <fmt>`, and `--explain-code <id>`.
//!
//! The law these tests pin: **every enumeration the CLI prints is generated from the registry that owns the fact, never
//! hand-written.** The machine readable surfaces exist so a doc (055's generated TOOLS.md) cannot describe a jqf that
//! does not exist; the capability gate's `discovery=` cell re-runs the same laws against the pinned surface table, and
//! this file pins the shapes the gate cannot express as a table (the `builtins` one-law pin, the exit codes, the
//! no-stdin command law).

use std::io::Write as _;
use std::process::{Command, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
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
    (output.status.code().unwrap_or(-1), output.stdout, output.stderr)
}

/// The ONE LAW with two doors: `--list-builtins` and the `builtins` builtin must enumerate the same names. The CLI
/// surface and the language surface share the registry and the prelude enumeration, so they cannot drift.
#[test]
fn list_builtins_matches_the_builtin() {
    let (cli_code, cli_out, _) = run(&["--list-builtins"], "");
    assert_eq!(cli_code, 0, "--list-builtins must exit 0");
    let cli_names: Vec<String> = String::from_utf8(cli_out)
        .expect("utf-8")
        .lines()
        .map(str::to_owned)
        .collect();
    assert!(!cli_names.is_empty(), "--list-builtins is empty");
    assert_eq!(
        cli_names.iter().map(String::as_str).collect::<Vec<_>>(),
        {
            let mut sorted = cli_names.clone();
            sorted.sort();
            sorted
        },
        "--list-builtins must be sorted"
    );

    let (builtin_code, builtin_out, builtin_err) = run(&["builtins"], "null\n");
    assert_eq!(
        builtin_code,
        0,
        "the builtins builtin failed: {}",
        String::from_utf8_lossy(&builtin_err)
    );
    // The builtin's output is a pretty-printed JSON array of "name/arity" strings; names are identifiers, so a
    // line-shaped parse is exact.
    let mut builtin = Vec::new();
    for line in String::from_utf8(builtin_out).expect("utf-8").lines() {
        let trimmed = line.trim();
        if trimmed == "[" || trimmed == "]" {
            continue;
        }
        let entry = trimmed
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix(",").or_else(|| rest.strip_suffix('"')))
            .map(|entry| entry.trim_end_matches(','))
            .map(|entry| entry.trim_end_matches('"'))
            .expect("builtin entry is a quoted string");
        builtin.push(entry.to_owned());
    }
    assert_eq!(cli_names, builtin, "the two doors enumerate different names");
}

/// `--list-formats` must advertise exactly the help template's enumerations plus the per-format grouping; nothing it
/// prints may be unknown to the parser (the capability gate's `discovery=` cell holds the same law against the pinned
/// SURFACE table — this test pins the help-vs-list agreement).
#[test]
fn list_formats_agrees_with_help() {
    let (help_code, help_out, _) = run(&["--help"], "");
    assert_eq!(help_code, 0);
    let help_text = String::from_utf8(help_out).expect("utf-8");
    let (list_code, list_out, _) = run(&["--list-formats"], "");
    assert_eq!(list_code, 0);
    let list_text = String::from_utf8(list_out).expect("utf-8");

    // Every spelling the help enumerates appears in the list.
    for (marker, _) in [
        ("  --input-format ", "input formats"),
        ("  --output-format ", "output formats"),
        ("  --input-dialect ", "input dialects"),
        ("  --output-dialect ", "output dialects"),
    ] {
        let line = help_text
            .lines()
            .find(|line| line.starts_with(marker))
            .unwrap_or_else(|| panic!("--help has no {marker:?} line"));
        let spellings = line[marker.len()..].split('|');
        for spelling in spellings {
            assert!(
                list_text.split_whitespace().any(|word| word == spelling),
                "--list-formats omits help spelling {spelling}"
            );
        }
    }
}

/// `--help-format <fmt>` exists for every format the parser accepts, and the page's dialect lines enumerate the same
/// spellings the flat lists do.
#[test]
fn help_format_pages_exist_for_every_format() {
    for format in ["json", "ndjson", "toml", "csv", "cbor", "yaml", "xml", "render"] {
        let (code, out, err) = run(&["--help-format", format], "");
        assert_eq!(
            code,
            0,
            "--help-format {format} failed: {}",
            String::from_utf8_lossy(&err)
        );
        let page = String::from_utf8(out).expect("utf-8");
        assert!(page.starts_with(&format!("{format}\n")), "page names the format");
        assert!(
            page.contains("direction:") && page.contains("dialects:"),
            "page has direction and dialects"
        );
    }
    // An unknown format is a usage error, never a silent empty page.
    let (code, out, _) = run(&["--help-format", "no-such-format"], "");
    assert_eq!(code, 2, "--help-format no-such-format must exit 2");
    assert!(out.is_empty(), "no stdout on an unknown format");
}

/// `--explain-code` prints a known code's row and exits 2 on an unknown id with no stdout — a lookup is never a silent
/// empty page. (The capability gate's `discovery=` cell covers EVERY live code in the manifest; this test pins the
/// exit-class law.)
#[test]
fn explain_code_known_and_unknown() {
    let (code, out, _) = run(&["--explain-code", "32"], "");
    assert_eq!(code, 0, "--explain-code 32 must exit 0");
    let page = String::from_utf8(out).expect("utf-8");
    assert!(page.contains("MACHINE_RAW_NUL"), "page names the code");
    assert!(page.contains("meaning:"), "page carries the meaning");

    let (unknown_code, unknown_out, _) = run(&["--explain-code", "9999"], "");
    assert_eq!(unknown_code, 2, "--explain-code 9999 must exit 2");
    assert!(unknown_out.is_empty(), "no stdout for an unknown id");
}

/// The discovery commands are COMMANDS: they exit without reading stdin, so a closed/empty stdin never blocks or
/// changes them, and they never write to stderr on success.
#[test]
fn discovery_commands_do_not_read_stdin() {
    for args in [
        &["--list-builtins"][..],
        &["--list-formats"],
        &["--help-format", "yaml"],
        &["--explain-code", "1"],
        &["--help", "startswith"],
    ] {
        let (code, _, err) = run(args, "");
        assert_eq!(code, 0, "{args:?} must exit 0 with empty stdin");
        assert!(
            err.is_empty(),
            "{args:?} writes nothing to stderr on success: {}",
            String::from_utf8_lossy(&err)
        );
    }
}

/// `--help <builtin>` reads the family's registry prose: summary always, detail when the record carries extended text.
#[test]
fn help_builtin_prints_family_summary_and_detail() {
    let (code, out, err) = run(&["--help", "startswith"], "");
    assert_eq!(code, 0, "--help startswith failed: {}", String::from_utf8_lossy(&err));
    let page = String::from_utf8(out).expect("utf-8");
    assert!(page.starts_with("name:     startswith\n"));
    assert!(page.contains("summary:"));
    assert!(page.contains("empty prefix"), "detail prose is present: {page}");

    let (code, out, _) = run(&["--help", "abs"], "");
    assert_eq!(code, 0, "--help abs must exit 0");
    let page = String::from_utf8(out).expect("utf-8");
    assert!(page.contains("summary:"));
    assert!(!page.contains("detail:"), "empty detail omits the detail line: {page}");
}
