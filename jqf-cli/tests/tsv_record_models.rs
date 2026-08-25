//! The TSV surface: extension detection, explicit-format override, the delimiter-dial refusal, and the JSON round trip.
//!
//! jq has no TSV at all, so nothing here can be pinned against the compat corpus — these are jqf's own decided laws
//! (134, ruled 2026-08-15).

use std::process::{Command, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// Runs `jqf args…` with stdin REDIRECTED from a regular file whose name is `name` (the seekable shape the
/// extension-detection tests need).
fn run_file(args: &[&str], name: &str, contents: &str) -> (i32, String, String) {
    let dir = std::env::temp_dir().join(format!(
        "jqf-tsv-model-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::create_dir_all(&dir).expect("creates the fixture dir");
    let path = dir.join(name);
    std::fs::write(&path, contents).expect("writes the fixture");
    let output = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(args)
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("jqf runs to completion");
    let _ = std::fs::remove_dir_all(&dir);
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stdout).expect("stdout is UTF-8"),
        String::from_utf8(output.stderr).expect("stderr is UTF-8"),
    )
}

#[test]
fn a_tsv_file_decodes_with_no_format_flag() {
    // 134's goal: `jqf. data.tsv` works with no flag. The `.tsv` extension resolves to the registered `tsv` format,
    // whose default dialect splits on tabs and keeps quotes as data.
    let (code, out, err) = run_file(&["-c", "."], "data.tsv", "name\tnote\nada\t\"b\"\nbob\tplain\n");
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        out,
        "[\"name\",\"note\"]\n[\"ada\",\"\\\"b\\\"\"]\n[\"bob\",\"plain\"]\n"
    );
}

#[test]
fn an_explicit_input_format_still_wins_over_the_extension() {
    // `--input-format json` names the format; the `.tsv` extension is not a dialect and must not fight it (the 112
    // detection law).
    let (code, out, err) = run_file(&["--input-format", "json", "-c", "."], "data.tsv", "[1,2,3]");
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "[1,2,3]\n");
}

#[test]
fn the_delimiter_dial_is_refused_for_tsv_input() {
    // 134 D4: the TSV grammar binds its own tab delimiter, so `--csv-delimiter` with tsv is a usage error before a byte
    // is read — never a silently ignored dial.
    let (code, _, err) = run_file(
        &["--input-format", "tsv", "--csv-delimiter", ",", "-c", "."],
        "data.tsv",
        "a\tb\n",
    );
    assert_eq!(code, 2, "the dial must be refused for TSV input");
    assert!(err.contains("CSV input only"), "{err}");
}

#[test]
fn a_tsv_table_round_trips_through_json() {
    // TSV -> JSON -> TSV: decode under `tsv.utf8@1`, re-encode under `tsv.jqf-lf@1`, byte-identical because the table
    // is representable.
    let table = "name\tnote\nada\tplain\nbob\t\"quoted\"\n";
    let (code, out, err) = run_file(
        &["--input-format", "tsv", "--output-format", "tsv", "-c", "."],
        "data.tsv",
        table,
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, table, "a representable TSV table round-trips");
}

#[test]
fn crlf_framed_records_decode_with_the_lf_row_count() {
    // The framing authority ends a TSV record at LF or CRLF, so a CRLF table decodes to exactly its LF twin's rows —
    // and the incremental cut/count walks mirror that same law.
    let (code, out, err) = run_file(&["--input-format", "tsv", "-c", "."], "data.tsv", "a\tb\r\nc\td\r\n");
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "[\"a\",\"b\"]\n[\"c\",\"d\"]\n");
}

#[test]
fn a_bare_cr_is_a_terminal_framing_fault_not_a_terminator() {
    // boundary.rs's law: LF and CRLF terminate; a lone CR is a fault. A CR-framed table is refused at its first bare CR
    // with nothing past it published — never silently split into records at each CR.
    let (code, out, err) = run_file(&["--input-format", "tsv", "-c", "."], "data.tsv", "a\tb\rc\td\r");
    assert_ne!(code, 0, "a bare CR must refuse the stream");
    assert_eq!(out, "", "nothing is published past the framing fault");
    assert!(err.contains("carriage return"), "{err}");
}
