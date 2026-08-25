//! The D3 CSV surface: the three input models over a record stream, the headered dialect pair, and the fences that
//! stay.
//!
//! jq has no CSV INPUT at all, so none of this can be pinned against the compat corpus's jq oracle — these are jqf's
//! own decided laws (see `docs/architecture/contracts.md`).

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
    if let Err(error) = child.stdin.take().expect("stdin is piped").write_all(stdin.as_bytes()) {
        // A usage-error child exits WITHOUT reading stdin, which closes the pipe mid-write; BrokenPipe is the expected
        // race there, not a test failure (surfaced by the 003 linux-amd64 emulated lane, where the child's exit
        // reliably beats the parent's write).
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

fn stdout(args: &[&str], stdin: &str) -> String {
    let (code, out, err) = run(args, stdin);
    assert_eq!(code, 0, "expected success for {args:?}: {err}");
    out
}

/// Runs `jqf args…` with stdin REDIRECTED from a regular file — the seekable shape, which reads whole and serves the
/// headered dialect.
///
/// The headered CSV dialect is a usage error on the piped (non-seekable) stream route — the header row is a
/// whole-stream fact the cycle drive cannot carry across refills — so the headered per-record laws below are pinned
/// through the seekable form, which runs the same record drive over the whole input.
fn run_seekable(args: &[&str], stdin: &str) -> (i32, String, String) {
    let path = std::env::temp_dir().join(format!(
        "jqf-csv-model-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, stdin).expect("writes the fixture");
    let output = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(args)
        .stdin(Stdio::from(std::fs::File::open(&path).expect("opens the fixture")))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("jqf runs to completion");
    let _ = std::fs::remove_file(&path);
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stdout).expect("stdout is UTF-8"),
        String::from_utf8(output.stderr).expect("stderr is UTF-8"),
    )
}

fn headered_run_seekable(extra: &[&str], stdin: &str) -> String {
    let owned = headered(extra);
    let args: Vec<&str> = owned.iter().map(String::as_str).collect();
    let (code, out, err) = run_seekable(&args, stdin);
    assert_eq!(code, 0, "expected success for {args:?}: {err}");
    out
}

const HEADERED: [&str; 4] = ["--input-format", "csv", "--input-dialect", "csv.rfc4180-header@1"];

fn headered(extra: &[&str]) -> Vec<String> {
    HEADERED
        .iter()
        .chain(extra.iter())
        .map(|argument| (*argument).to_owned())
        .collect()
}

fn headered_run(extra: &[&str], stdin: &str) -> String {
    let owned = headered(extra);
    let args: Vec<&str> = owned.iter().map(String::as_str).collect();
    stdout(&args, stdin)
}

#[test]
fn slurp_aggregates_across_rows() {
    // The ruling's own acceptance case: sum a column in ONE process, which is what `mlr`/`qsv` do and what the
    // two-process pipeline existed for.
    assert_eq!(
        headered_run(&["-c", "-s", "map(.age|tonumber)|add"], "name,age\nada,37\nbob,34\n"),
        "71\n"
    );
}

#[test]
fn null_input_streams_the_records_through_inputs() {
    assert_eq!(
        headered_run(&["-c", "-n", "[inputs]"], "name,age\nada,37\nbob,34\n"),
        "[{\"name\":\"ada\",\"age\":\"37\"},{\"name\":\"bob\",\"age\":\"34\"}]\n"
    );
}

#[test]
fn slurp_over_the_array_dialect_collects_bare_rows() {
    // The DEFAULT dialect is untouched by the headered one: every record, header row included, is still an array of its
    // fields.
    assert_eq!(
        stdout(&["--input-format", "csv", "-c", "-s", "."], "name,age\nada,37\n"),
        "[[\"name\",\"age\"],[\"ada\",\"37\"]]\n"
    );
}

#[test]
fn the_default_dialect_still_publishes_arrays_per_record() {
    assert_eq!(
        stdout(&["--input-format", "csv", "-c", "."], "name,age\nada,37\n"),
        "[\"name\",\"age\"]\n[\"ada\",\"37\"]\n"
    );
}

#[test]
fn the_headered_dialect_publishes_objects_and_keeps_cells_as_strings() {
    // The headered per-record laws ride the SEEKABLE form: the piped stream route refuses the headered dialect (see
    // `run_seekable`), and the whole- read record drive over a file serves the same record model.
    assert_eq!(
        headered_run_seekable(&["-c", "."], "name,age\nada,37\n"),
        "{\"name\":\"ada\",\"age\":\"37\"}\n"
    );
    // NO numeric inference: `tonumber` stays explicit.
    assert_eq!(
        headered_run_seekable(&["-c", ".age|type"], "name,age\nada,37\n"),
        "\"string\"\n"
    );
}

#[test]
fn a_column_is_named_by_its_header_key() {
    assert_eq!(
        headered_run_seekable(&["-c", ".age"], "name,age\nada,37\nbob,34\n"),
        "\"37\"\n\"34\"\n"
    );
}

#[test]
fn the_headered_dialects_round_trip_a_table() {
    let table = "name,note\nada,\"a,b\"\nbob,plain\n";
    let owned = headered(&[
        "--output-format",
        "csv",
        "--output-dialect",
        "csv.jqf-rfc4180-header@1",
        ".",
    ]);
    let args: Vec<&str> = owned.iter().map(String::as_str).collect();
    let (code, out, err) = run_seekable(&args, table);
    assert_eq!(code, 0, "{err}");
    // 139 DEF2: the RFC-named CSV output dialect appends CRLF, so the round trip normalizes the input's LF terminators.
    assert_eq!(out, "name,note\r\nada,\"a,b\"\r\nbob,plain\r\n");
}

#[test]
fn a_ragged_row_fails_the_record_stream() {
    let owned = headered(&["-c", "."]);
    let args: Vec<&str> = owned.iter().map(String::as_str).collect();
    let (code, out, _) = run_seekable(&args, "a,b\n1,2\n3\n");
    assert_eq!(code, 5, "a ragged row is a terminal record fault");
    // The rows before the fault are published, exactly as every other record fault publishes its clean prefix.
    assert_eq!(out, "{\"a\":\"1\",\"b\":\"2\"}\n");
}

#[test]
fn duplicate_header_names_take_a_numeric_suffix() {
    assert_eq!(
        headered_run_seekable(&["-c", "."], "a,a\n1,2\n"),
        "{\"a\":\"1\",\"a_2\":\"2\"}\n"
    );
}

#[test]
fn a_record_independent_program_runs_without_building() {
    // `empty` is record-independent: the record drive lowers the ordinary whole-document requirement and the lazy
    // binding never materializes a value the program does not read, so each record is validated to the codec's full
    // strictness and nothing is published. The outcome — no bytes, exit 0 — is identical to the record route's answer
    // for the same program, whatever the decode path that served it.
    let (code, out, err) = run(&["--input-format", "csv", "empty"], "a,b\n1,2\n3,4\n");
    assert_eq!(
        code, 0,
        "`empty` must be a proper record-independent program over CSV: {err}"
    );
    assert_eq!(out, "", "`empty` publishes nothing");

    // The headered dialect serves the same rung, and a ragged row still fails it exactly as it fails the decode route
    // (the validation walk enforces the header's field count).
    let (code, out) = headered_run_seekable_result(&["empty"], "a,b\n1,2\n3\n");
    assert_eq!(code, 5, "a ragged row must fail the validation route too");
    assert_eq!(out, "", "nothing publishes before the validation fault");

    // The record-independent `null` literal rides the same rung.
    let (code, out, err) = run(&["--input-format", "csv", "null"], "a,b\n1,2\n");
    assert_eq!(code, 0, "`null` over CSV: {err}");
    assert_eq!(out, "null\nnull\n");
}

fn headered_run_seekable_result(extra: &[&str], stdin: &str) -> (i32, String) {
    let owned = headered(extra);
    let args: Vec<&str> = owned.iter().map(String::as_str).collect();
    let (code, out, _) = run_seekable(&args, stdin);
    (code, out)
}

#[test]
fn raw_input_over_csv_publishes_one_json_string_per_line() {
    // `-R` rewrites the input into JSON string literals BEFORE any route decision, so the record framing is gone and
    // the named format stops describing the bytes — the same resolution `--input-format yaml -R` exhibits (2026-08-04).
    assert_eq!(
        stdout(&["--input-format", "csv", "-c", "-R", "."], "a,b\n1,2\n"),
        "\"a,b\"\n\"1,2\"\n"
    );
    assert_eq!(
        stdout(&["--input-format", "csv", "-c", "-R", "-s", "."], "a,b\n1,2\n"),
        "\"a,b\\n1,2\\n\"\n"
    );
}

#[test]
fn follow_still_refuses_the_three_input_models() {
    // `-R`/`-s` read the input to completion and are refused up front.
    for flag in ["-R", "-s"] {
        let (code, _, err) = run(&["--follow", flag, "."], "");
        assert_eq!(code, 2, "--follow {flag} must be a usage error");
        assert!(
            err.contains("--follow's live record stream"),
            "unexpected message for {flag}: {err}"
        );
    }
    // `-n --follow` is served for input-family programs (the live-window shape, gate 3); a non-input program — which
    // would run once over null and end the tail — is refused after compile with its own message, still exit 2.
    let (code, _, err) = run(&["--follow", "-n", "."], "");
    assert_eq!(code, 2, "-n --follow with a non-input program is a usage error");
    assert!(
        err.contains("pull `inputs`/`input`"),
        "unexpected message for -n: {err}"
    );
}

#[test]
fn the_headered_output_dialect_requires_csv_output() {
    let (code, _, err) = run(&["--output-dialect", "csv.jqf-rfc4180-header@1", "."], "{\"a\":1}");
    assert_eq!(code, 2);
    assert!(
        err.contains("invalid output format/dialect pair"),
        "unexpected message: {err}"
    );
}

#[test]
fn the_help_text_lists_every_parsed_csv_dialect() {
    // The sealed-id lists and the parser's match arms must not drift: a dialect the parser accepts but `--help` never
    // names is unreachable in practice.
    let help = stdout(&["--help"], "");
    for id in [
        "csv.rfc4180@1",
        "csv.rfc4180-header@1",
        "csv.utf8@1",
        "csv.utf8-header@1",
        "csv.jqf-rfc4180@1",
        "csv.jqf-rfc4180-header@1",
        "csv.jqf-utf8@1",
        "csv.jqf-utf8-header@1",
    ] {
        assert!(help.contains(id), "--help does not name {id}");
    }
}

///  the short `--input-format csv` selects the
/// Unicode-capable `csv.utf8@1` family, so a non-English file keeps decoding; the explicit RFC-named dialect freezes
/// TEXTDATA and rejects the same bytes.
#[test]
fn the_short_csv_format_accepts_utf8_and_the_rfc_dialect_rejects_it() {
    let table = b"caf\xc3\xa9,1\n2,3\n";
    let table = core::str::from_utf8(table).expect("fixture is UTF-8");

    // The default: UTF-8 cells decode.
    let out = stdout(&["--input-format", "csv", "-c", "."], table);
    assert_eq!(out, "[\"café\",\"1\"]\n[\"2\",\"3\"]\n");

    // The explicit RFC opt-in: a non-ASCII cell is invalid input even though its bytes are valid UTF-8.
    let (code, _, err) = run_seekable(
        &["--input-format", "csv", "--input-dialect", "csv.rfc4180@1", "-c", "."],
        table,
    );
    // The exact class: an invalid INPUT is the runtime decode class (5), never a bare nonzero that a usage regression
    // would also satisfy.
    assert_eq!(code, 5, "--input-dialect csv.rfc4180@1 must reject é");
    assert!(!err.is_empty());

    // The headered utf8 twin admits it too (`--header` selects utf8).
    let (code, out, err) = run_seekable(
        &["--header", "--input-format", "csv", "-c", "."],
        "name,dish\nada,crème brûlée\n",
    );
    assert_eq!(code, 0, "headered utf8 decode succeeds: {err}");
    assert_eq!(out, "{\"name\":\"ada\",\"dish\":\"crème brûlée\"}\n");
}

#[test]
fn a_single_run_model_plans_serial() {
    // The clean-morsel law: `-s` runs the program ONCE over the whole stream, so there is no per-record morsel to
    // relay.
    let owned = headered(&["--diagnostics", "-c", "-s", "length"]);
    let args: Vec<&str> = owned.iter().map(String::as_str).collect();
    let (code, out, err) = run(&args, "a,b\n1,2\n");
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "1\n");
    assert!(
        err.contains("single-run-model"),
        "expected the single-run plan decision: {err}"
    );
}

#[test]
fn a_headered_output_plans_serial() {
    let owned = headered(&[
        "--output-format",
        "csv",
        "--output-dialect",
        "csv.jqf-rfc4180-header@1",
        "--diagnostics",
        ".",
    ]);
    let args: Vec<&str> = owned.iter().map(String::as_str).collect();
    let (code, out, err) = run_seekable(&args, "a,b\n1,2\n");
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "a,b\r\n1,2\r\n");
    assert!(
        err.contains("stateful-output"),
        "expected the stateful-output plan decision: {err}"
    );
}

#[test]
fn the_headered_output_dialect_refuses_a_bare_row() {
    // A bare array row has no keys to publish a header FROM. The array dialect's own output profile
    // (`csv.jqf-rfc4180@1`) is what encodes it.
    let (code, out, _) = run(
        &[
            "--input-format",
            "csv",
            "--output-format",
            "csv",
            "--output-dialect",
            "csv.jqf-rfc4180-header@1",
            ".",
        ],
        "a,b\n1,2\n",
    );
    // The exact class: an unrepresentable VALUE is the runtime encode class (5), never a bare nonzero that a usage
    // regression would also satisfy.
    assert_eq!(code, 5, "a keyless row must not publish a header");
    assert_eq!(out, "");
}

/// The 089 §2 delimiter dial: `--csv-delimiter` reaches the framer in BOTH directions — a TSV stream splits on tabs,
/// and the record route's output encodes with the same delimiter the request read. TSV is the recorded second dialect;
/// the flag is the CLI spelling of it.
#[test]
fn the_delimiter_dial_reaches_the_framer_both_directions() {
    // TSV input: fields split on the tab, not the comma.
    let out = stdout(
        &["--input-format", "csv", "--csv-delimiter", r"\t", "-c", "."],
        "a\tb\n1\t2\n",
    );
    assert_eq!(out, "[\"a\",\"b\"]\n[\"1\",\"2\"]\n", "tab-delimited rows");

    // Pipe input.
    let out = stdout(
        &["--input-format", "csv", "--csv-delimiter", "|", "-c", "."],
        "a|b\n1|2\n",
    );
    assert_eq!(out, "[\"a\",\"b\"]\n[\"1\",\"2\"]\n", "pipe-delimited rows");

    // An actual tab byte (the shell's $'\t') works identically.
    let out = stdout(
        &["--input-format", "csv", "--csv-delimiter", "\t", "-c", "."],
        "a\tb\n1\t2\n",
    );
    assert_eq!(out, "[\"a\",\"b\"]\n[\"1\",\"2\"]\n", "literal tab byte");

    // CSV output encodes with the delimiter the request read: a record row written back through the same dial
    // round-trips.
    let out = stdout(
        &[
            "--input-format",
            "csv",
            "--csv-delimiter",
            r"\t",
            "--output-format",
            "csv",
            "--output-dialect",
            "csv.jqf-rfc4180@1",
            ".",
        ],
        "a\tb\n1\t2\n",
    );
    assert_eq!(out, "a\tb\r\n1\t2\r\n", "tab-delimited round-trip (139 DEF2: CRLF)");

    // An invalid delimiter is a usage error, and the dial is CSV-only.
    let (code, _, err) = run(&["--input-format", "csv", "--csv-delimiter", "xx", "-c", "."], "a,b\n");
    assert_eq!(code, 2, "a two-byte delimiter is a usage error");
    assert!(err.contains("single ASCII byte"), "{err}");
    let (code, _, err) = run(&["--csv-delimiter", ",", "-c", "."], "1\n");
    assert_eq!(code, 2, "the dial applies to CSV input only");
    assert!(err.contains("CSV input only"), "{err}");
}

#[test]
fn crlf_framed_records_decode_with_the_lf_row_count() {
    // The framing authority ends a record at LF or CRLF; the incremental cut/count walks mirror that law, so both
    // spellings frame the same two records.
    let (code, out, err) = run(&["--input-format", "csv", "-c", "."], "a,b\r\nc,d\r\n");
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "[\"a\",\"b\"]\n[\"c\",\"d\"]\n");
}

#[test]
fn a_quoted_record_and_its_successor_frame_in_order() {
    // A record whose quoted field holds raw line feeds ends at its out-of-quote terminator, and the NEXT record still
    // frames with the next ordinal: `base_record + relative` stays cycle-invariant against the whole-input drive over
    // identical bytes.
    let out = stdout(&["--input-format", "csv", "-c", "."], "a,\"b\nc\",d\ne,f\n");
    assert_eq!(out, "[\"a\",\"b\\nc\",\"d\"]\n[\"e\",\"f\"]\n");
}

/// `--header`: the one-word spelling of the headered dialect pair. A CSV user's first instinct is `.name`, not `.[3]`,
/// and the 38-character `--input-dialect csv.rfc4180-header@1` was the only way to say it. The flag selects the
/// headered dialect on whichever CSV sides the request has, and leaves the array default alone for everyone who did not
/// ask.
#[test]
fn the_csv_header_dial_selects_the_headered_dialect_on_both_sides() {
    // Input: rows decode as objects keyed by row 1.
    let (code, out, err) = run_seekable(
        &["--header", "--input-format", "csv", "-c", "."],
        "name,salary\nada,10\ngrace,20\n",
    );
    assert_eq!(code, 0, "headered decode succeeds: {err}");
    assert_eq!(
        out, "{\"name\":\"ada\",\"salary\":\"10\"}\n{\"name\":\"grace\",\"salary\":\"20\"}\n",
        "rows are objects keyed by the header"
    );

    // A field reads by NAME, which is the whole point of the dial.
    let (_, out, _) = run_seekable(
        &["--header", "--input-format", "csv", "-c", ".salary"],
        "name,salary\nada,10\n",
    );
    assert_eq!(out, "\"10\"\n");

    // Output: the encode mirror rides along, so CSV in / CSV out round-trips through one flag instead of two dialect
    // spellings.
    let (_, out, _) = run_seekable(
        &["--header", "--input-format", "csv", "--output-format", "csv", "."],
        "name,salary\nada,10\n",
    );
    assert_eq!(out, "name,salary\r\nada,10\r\n", "headered round-trip (139 DEF2: CRLF)");
}

#[test]
fn the_csv_header_dial_leaves_the_array_default_alone() {
    // Without the flag the array dialect still serves — including on the piped route, which the headered dialect cannot
    // serve at all.
    let out = stdout(&["--input-format", "csv", "-c", "."], "a,b\n1,2\n");
    assert_eq!(out, "[\"a\",\"b\"]\n[\"1\",\"2\"]\n", "the default is unchanged");
}

#[test]
fn the_csv_header_dial_refuses_what_it_cannot_mean() {
    // A contradiction with the explicit array dialect is named, never silently resolved in either direction.
    let (code, _, err) = run_seekable(
        &[
            "--header",
            "--input-format",
            "csv",
            "--input-dialect",
            "csv.rfc4180@1",
            "-c",
            ".",
        ],
        "a,b\n1,2\n",
    );
    assert_eq!(code, 2, "the contradiction is a usage error");
    assert!(err.contains("--header contradicts"), "{err}");

    // A request with no CSV side at all is a mistake, not a no-op.
    let (code, _, err) = run(&["--header", "-c", "."], "1\n");
    assert_eq!(code, 2, "the dial needs a CSV side");
    assert!(err.contains("CSV input or output only"), "{err}");

    // The piped route still refuses the headered dialect and still names the redirect that reads whole — the flag does
    // not paper over the ceiling.
    let (code, _, err) = run(&["--header", "--input-format", "csv", "-c", "."], "a\n1\n");
    assert_eq!(code, 2, "a pipe cannot serve the headered dialect");
    assert!(err.contains("redirect stdin from a regular file"), "{err}");
}

#[test]
fn headered_query_encode_writes_the_header_row() {
    // `--header` without `--edit` is ordinary query encode: the catalog factory must write the header. The seekable
    // form serves the headered dialect (a pipe refuses it).
    let (code, out, err) = run_seekable(
        &["--header", "--input-format", "csv", "--output-format", "csv", "."],
        "name,note\nada,\"a,b\"\n",
    );
    assert_eq!(code, 0, "headered query encode succeeds: {err}");
    assert_eq!(out, "name,note\r\nada,\"a,b\"\r\n", "{err}");
}

#[test]
fn headered_edit_identity_is_byte_identical() {
    let (code, out, err) = run_seekable(
        &[
            "--header",
            "--input-format",
            "csv",
            "--output-format",
            "csv",
            "--edit",
            ".",
        ],
        "name,age\nada,37\n",
    );
    assert_eq!(code, 0, "headered --edit identity succeeds: {err}");
    assert_eq!(out, "name,age\nada,37\n", "{err}");
}

#[test]
fn headered_input_dialect_edit_without_header_flag_keeps_the_header() {
    // Headered INPUT, array OUTPUT, `--edit`: identity still publishes the authored header prefix, then the data-record
    // payloads.
    let (code, out, err) = run_seekable(
        &[
            "--input-format",
            "csv",
            "--input-dialect",
            "csv.rfc4180-header@1",
            "--output-format",
            "csv",
            "--edit",
            ".",
        ],
        "name,age\nada,37\n",
    );
    assert_eq!(code, 0, "headered-input --edit identity succeeds: {err}");
    assert_eq!(out, "name,age\nada,37\n", "{err}");
}

#[test]
fn headered_in_place_edit_writes_the_file() {
    let path = std::env::temp_dir().join(format!(
        "jqf-csv-headered-inplace-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let original = b"name,age\nada,37\n";
    std::fs::write(&path, original).expect("writes the fixture");
    let output = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args([
            "--header",
            "--input-format",
            "csv",
            "--output-format",
            "csv",
            "--edit",
            "--in-place",
            ".",
            path.to_str().expect("path is UTF-8"),
        ])
        .output()
        .expect("jqf runs to completion");
    let written = std::fs::read(&path).expect("file still exists");
    let _ = std::fs::remove_file(&path);
    assert_eq!(
        output.status.code(),
        Some(0),
        "headered --in-place --edit writes: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(written, original, "identity --edit is byte-identical");

    let path = std::env::temp_dir().join(format!(
        "jqf-csv-headered-inplace-cell-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let original = b"name,age\r\nada,37\r\n";
    std::fs::write(&path, original).expect("writes the fixture");
    let output = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args([
            "--header",
            "--input-format",
            "csv",
            "--output-format",
            "csv",
            "--edit",
            "--in-place",
            r#".age = "38""#,
            path.to_str().expect("path is UTF-8"),
        ])
        .output()
        .expect("jqf runs to completion");
    let written = std::fs::read(&path).expect("file still exists");
    let _ = std::fs::remove_file(&path);
    assert_eq!(
        output.status.code(),
        Some(0),
        "headered --in-place cell assignment writes: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        written, b"name,age\r\nada,38\r\n",
        "cell assignment keeps the authored header terminator"
    );
}

#[test]
fn array_csv_in_place_edit_writes_the_file() {
    // Record `--edit` publishes through the CLI sink. `--in-place` must open that sink on the file, or the splice
    // prints to stdout and the file stays untouched.
    let path = std::env::temp_dir().join(format!(
        "jqf-csv-array-inplace-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, b"ada,37\n").expect("writes the fixture");
    let output = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args([
            "--input-format",
            "csv",
            "--output-format",
            "csv",
            "--edit",
            "--in-place",
            ".[1] = \"99\"",
            path.to_str().expect("path is UTF-8"),
        ])
        .output()
        .expect("jqf runs to completion");
    let written = std::fs::read(&path).expect("file still exists");
    let _ = std::fs::remove_file(&path);
    assert!(
        output.status.success(),
        "array csv --in-place --edit succeeds: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "in-place publishes to the file, not stdout: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        written, b"ada,99\n",
        "the edited cell is written back and the authored terminator is kept"
    );
}

#[test]
fn array_tsv_in_place_edit_writes_the_file() {
    let path = std::env::temp_dir().join(format!(
        "jqf-tsv-array-inplace-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, b"ada\t37\n").expect("writes the fixture");
    let output = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args([
            "--input-format",
            "tsv",
            "--output-format",
            "tsv",
            "--edit",
            "--in-place",
            ".[1] = \"99\"",
            path.to_str().expect("path is UTF-8"),
        ])
        .output()
        .expect("jqf runs to completion");
    let written = std::fs::read(&path).expect("file still exists");
    let _ = std::fs::remove_file(&path);
    assert!(
        output.status.success(),
        "array tsv --in-place --edit succeeds: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty(), "in-place publishes to the file");
    assert_eq!(written, b"ada\t99\n", "the edited cell is written back");
}

#[test]
fn array_csv_edit_output_writes_the_destination() {
    let dest = std::env::temp_dir().join(format!(
        "jqf-csv-edit-output-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let output = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args([
            "--input-format",
            "csv",
            "--output-format",
            "csv",
            "--edit",
            "--output",
            dest.to_str().expect("path is UTF-8"),
            ".[1] = \"99\"",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("jqf spawns");
    // Seekable-style: write via stdin pipe is fine for array CSV.
    let mut child = output;
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(b"ada,37\n")
        .expect("input writes");
    let finished = child.wait_with_output().expect("jqf runs to completion");
    let written = std::fs::read(&dest).expect("destination exists");
    let _ = std::fs::remove_file(&dest);
    assert!(
        finished.status.success(),
        "array csv --edit --output succeeds: {}",
        String::from_utf8_lossy(&finished.stderr)
    );
    assert!(
        finished.stdout.is_empty(),
        "--output must not also print: {:?}",
        String::from_utf8_lossy(&finished.stdout)
    );
    assert_eq!(
        written, b"ada,99\n",
        "the edited cell is written back and the authored terminator is kept"
    );
}
