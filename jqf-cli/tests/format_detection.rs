//!  implicit input-format detection for NAMED FILES.
//!
//! The ruling: a named file argument detects its input format implicitly by extension — `jqf. data.yaml` parses YAML,
//! no `--input-format` needed — while stdin stays json always (content sniffing is ambiguous: every JSON document is
//! valid YAML). `--input-format` keeps absolute override priority, and an unrecognized extension falls back to json, so
//! no currently-working invocation changes. The extension table itself is a codec registration fact, read through the
//! catalog (the 039 law); these tests pin the CLI behavior at the process boundary, and the sdk-smoke receipt pins the
//! table.

use std::process::{Command, Output, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// Runs `jqf args…` with `stdin` as the input, returning (exit code, stdout, stderr).
fn run(args: &[&str], stdin: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let output = run_bytes(args, stdin.as_bytes());
    (output.status.code().unwrap_or(-1), output.stdout, output.stderr)
}

/// Runs `jqf args…` with the given stdin bytes, returning the full Output.
fn run_bytes(args: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("jqf spawns");
    std::io::Write::write_all(&mut child.stdin.take().expect("stdin is piped"), stdin)
        .expect("input writes to jqf's stdin");
    child.wait_with_output().expect("jqf runs to completion")
}

/// Writes `bytes` to a uniquely named temp file with the given suffix.
fn write_temp(suffix: &str, bytes: &[u8]) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!(
        "jqf-format-detection-{}-{}.{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
        suffix
    ));
    std::fs::write(&path, bytes).expect("test input writes");
    path
}

fn stdout_text(args: &[&str], stdin: &str) -> String {
    let (code, out, err) = run(args, stdin);
    assert_eq!(code, 0, "exit 0: {}", String::from_utf8_lossy(&err));
    String::from_utf8(out).expect("utf-8 output")
}

/// A named `.yaml` file parses as YAML with no `--input-format`.
#[test]
fn yaml_file_detects_by_extension() {
    let path = write_temp("yaml", b"port: 8080\nname: svc\n");
    let out = stdout_text(&["-c", ".", path.to_str().unwrap()], "");
    assert_eq!(out, "{\"port\":8080,\"name\":\"svc\"}\n");
    let _ = std::fs::remove_file(&path);
}

/// A named `.yml` file is the YAML alias.
#[test]
fn yml_file_detects_as_yaml() {
    let path = write_temp("yml", b"a: 1\n");
    let out = stdout_text(&["-c", ".", path.to_str().unwrap()], "");
    assert_eq!(out, "{\"a\":1}\n");
    let _ = std::fs::remove_file(&path);
}

/// The same bytes on STDIN stay json: the YAML document is a JSON parse error, never silently sniffed.
#[test]
fn stdin_stays_json() {
    let (code, out, err) = run(&["-c", "."], "port: 8080\n");
    assert_eq!(code, 5, "YAML on stdin must not parse as YAML");
    assert!(out.is_empty(), "no stdout on a failed json parse");
    let err = String::from_utf8_lossy(&err);
    assert!(
        err.contains("expected one complete JSON value"),
        "the failure is the json parse failure: {err}"
    );
    assert!(
        !err.contains("yaml"),
        "the json failure must not name the yaml codec: {err}"
    );
}

/// `--input-format` keeps absolute override priority over detection.
#[test]
fn explicit_input_format_overrides_detection() {
    let path = write_temp("yaml", b"port: 8080\n");
    let (code, out, _) = run(&["-c", "--input-format", "json", ".", path.to_str().unwrap()], "");
    assert_eq!(code, 5, "yaml bytes forced to json must fail");
    assert!(out.is_empty(), "no stdout");
    let _ = std::fs::remove_file(&path);
}

///  the output mirror of the input law — a `--output` PATH
/// whose extension a registration declares infers the output format, and `-o FORMAT` is the short spelling of
/// `--output-format`. Explicit `--output-format` always wins; a no-extension `--output` stays JSON.
#[test]
fn output_path_extension_detects_the_output_format() {
    let input = write_temp("json", b"{\"a\": 1}\n");
    let out = write_temp("yaml", b"");
    let (code, stdout, stderr) = run(
        &[
            "-c",
            "--output",
            out.to_str().expect("path"),
            ".",
            input.to_str().expect("path"),
        ],
        "",
    );
    assert_eq!(code, 0, "inferred-yaml run: {}", String::from_utf8_lossy(&stderr));
    assert!(stdout.is_empty(), "output goes to the --output file");
    assert_eq!(
        std::fs::read_to_string(&out).expect("written yaml"),
        "a: 1\n",
        "the .yaml extension names the output format"
    );
    std::fs::remove_file(&input).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn explicit_output_format_beats_the_path_extension() {
    let input = write_temp("json", b"{\"a\": 1}\n");
    let out = write_temp("yaml", b"");
    let (code, _, stderr) = run(
        &[
            "-c",
            "--output-format",
            "json",
            "--output",
            out.to_str().expect("path"),
            ".",
            input.to_str().expect("path"),
        ],
        "",
    );
    assert_eq!(code, 0, "explicit json wins: {}", String::from_utf8_lossy(&stderr));
    assert_eq!(
        std::fs::read_to_string(&out).expect("written json"),
        "{\"a\":1}\n",
        "explicit --output-format beats the .yaml extension"
    );
    std::fs::remove_file(&input).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn config_output_format_does_not_beat_the_path_extension() {
    let dir = std::env::temp_dir().join(format!(
        "jqf-format-config-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    std::fs::write(dir.join(".jqf.toml"), "[defaults]\noutput-format = \"ndjson\"\n").expect("config");
    let input = write_temp("json", b"{\"a\": 1}\n");
    let out = write_temp("yaml", b"");
    let output = Command::new(jqf_binary())
        .env_remove("JQF_NO_CONFIG")
        .env("HOME", &dir)
        .current_dir(&dir)
        .args([
            "-c",
            "--output",
            out.to_str().expect("path"),
            ".",
            input.to_str().expect("path"),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("jqf runs");
    assert_eq!(
        output.status.code(),
        Some(0),
        "path-inferred yaml: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&out).expect("written yaml"),
        "a: 1\n",
        "the .yaml destination must beat a config ndjson preference"
    );
    std::fs::remove_file(&input).ok();
    std::fs::remove_file(&out).ok();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn short_o_is_the_output_format_spelling() {
    let input = write_temp("json", b"{\"a\": 1}\n");
    let (code, stdout, _stderr) = run(&["-c", "-o", "yaml", ".", input.to_str().expect("path")], "");
    assert_eq!(code, 0, "-o yaml run");
    assert_eq!(String::from_utf8(stdout).expect("utf-8"), "a: 1\n");
    std::fs::remove_file(&input).ok();
}

/// An unrecognized extension falls back to json (today's behavior).
#[test]
fn unknown_extension_falls_back_to_json() {
    let path = write_temp("txt", b"{\"a\":1}\n");
    let out = stdout_text(&["-c", ".", path.to_str().unwrap()], "");
    assert_eq!(out, "{\"a\":1}\n");
    let _ = std::fs::remove_file(&path);
}

/// A file with no extension at all is json (today's behavior).
#[test]
fn extensionless_file_is_json() {
    let path = write_temp("noext", b"{\"a\":1}\n");
    let out = stdout_text(&["-c", ".", path.to_str().unwrap()], "");
    assert_eq!(out, "{\"a\":1}\n");
    let _ = std::fs::remove_file(&path);
}

/// A `.json` file is json (the extension agrees with the default).
#[test]
fn json_file_is_json() {
    let path = write_temp("json", b"{\"a\":1}\n");
    let out = stdout_text(&["-c", ".", path.to_str().unwrap()], "");
    assert_eq!(out, "{\"a\":1}\n");
    let _ = std::fs::remove_file(&path);
}

/// Extension matching is case-insensitive (macOS/Windows map `data.YAML` and `data.yaml` to the same file).
#[test]
fn extension_matching_is_case_insensitive() {
    let path = write_temp("YAML", b"a: 1\n");
    let out = stdout_text(&["-c", ".", path.to_str().unwrap()], "");
    assert_eq!(out, "{\"a\":1}\n");
    let _ = std::fs::remove_file(&path);
}

/// The non-JSON family detects too: TOML, CSV, NDJSON, CBOR.
#[test]
fn toml_csv_ndjson_cbor_detect() {
    let toml = write_temp("toml", b"port = 8080\n");
    assert_eq!(
        stdout_text(&["-c", ".", toml.to_str().unwrap()], ""),
        "{\"port\":8080}\n"
    );
    let _ = std::fs::remove_file(&toml);

    let csv = write_temp("csv", b"a,b\n1,2\n");
    assert_eq!(
        stdout_text(&["-c", ".", csv.to_str().unwrap()], ""),
        "[\"a\",\"b\"]\n[\"1\",\"2\"]\n"
    );
    let _ = std::fs::remove_file(&csv);

    let ndjson = write_temp("ndjson", b"{\"a\":1}\n{\"a\":2}\n");
    assert_eq!(
        stdout_text(&["-c", ".", ndjson.to_str().unwrap()], ""),
        "{\"a\":1}\n{\"a\":2}\n"
    );
    let _ = std::fs::remove_file(&ndjson);

    let jsonl = write_temp("jsonl", b"{\"a\":1}\n");
    assert_eq!(stdout_text(&["-c", ".", jsonl.to_str().unwrap()], ""), "{\"a\":1}\n");
    let _ = std::fs::remove_file(&jsonl);

    // CBOR is binary: the two-byte encoding of `1`.
    let cbor = write_temp("cbor", &[0x01]);
    assert_eq!(stdout_text(&["-c", ".", cbor.to_str().unwrap()], ""), "1\n");
    let _ = std::fs::remove_file(&cbor);
}

/// A compatible explicit dialect composes with detection; a conflicting one is a usage error, never a silent override.
#[test]
fn input_dialect_composes_with_detection() {
    let path = write_temp("yaml", b"a: 1\n");
    let out = stdout_text(
        &["-c", "--input-dialect", "yaml.core@1", ".", path.to_str().unwrap()],
        "",
    );
    assert_eq!(out, "{\"a\":1}\n");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn conflicting_input_dialect_is_a_usage_error() {
    let path = write_temp("yaml", b"a: 1\n");
    let (code, out, err) = run(&["-c", "--input-dialect", "rfc8259", ".", path.to_str().unwrap()], "");
    assert_eq!(code, 2, "a json dialect on a detected yaml file is a usage error");
    assert!(out.is_empty(), "no stdout");
    assert!(
        String::from_utf8_lossy(&err).contains("invalid input format/dialect pair"),
        "the error names the conflict: {}",
        String::from_utf8_lossy(&err)
    );
    let _ = std::fs::remove_file(&path);
}

/// `--edit` on a named non-JSON file detects and mirrors the output format: the edited document is re-emitted as TOML
/// (seam 5), never json.
#[test]
fn edit_detects_and_mirrors_the_input_format() {
    let path = write_temp("toml", b"port = 8080\n");
    let output = run_bytes(&["--edit", ".port = 9090", path.to_str().unwrap()], b"");
    assert!(
        output.status.success(),
        "toml edit succeeds: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_file(&path);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "port = 9090\n",
        "the edited document is re-emitted in its detected format, not json"
    );
}

/// `--edit --in-place` rewrites the named file itself, in its detected format.
#[test]
fn edit_in_place_rewrites_in_the_detected_format() {
    let path = write_temp("toml", b"port = 8080\n");
    let output = run_bytes(&["--edit", "--in-place", ".port = 9090", path.to_str().unwrap()], b"");
    assert!(
        output.status.success(),
        "in-place toml edit succeeds: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    let written = std::fs::read(&path).expect("edited file exists");
    let _ = std::fs::remove_file(&path);
    assert_eq!(
        String::from_utf8_lossy(&written),
        "port = 9090\n",
        "the file is re-emitted in its detected format, not json"
    );
}

/// `--list-formats` and `--help-format` advertise the extensions from the registry: what the CLI prints is what
/// detection accepts.
#[test]
fn list_formats_and_help_format_print_extensions() {
    let (code, out, _) = run(&["--list-formats"], "");
    assert_eq!(code, 0);
    let text = String::from_utf8(out).expect("utf-8");
    for (format, extensions) in [
        ("json", "json"),
        ("yaml", "yaml yml"),
        ("ndjson", "ndjson jsonl"),
        ("toml", "toml"),
        ("csv", "csv"),
        ("cbor", "cbor"),
        ("xml", "xml"),
        ("html", "html htm"),
        ("jqft", "jqft"),
        ("jqfjson", "jqfjson"),
        ("jqfb", "jqfb"),
    ] {
        // The format's own block: the text after the format-name STANDALONE line, up to the next blank line.
        let section = text
            .split_once(&format!("\n{format}\n"))
            .map_or("", |(_, rest)| rest.split("\n\n").next().unwrap_or(""));
        assert!(
            section.contains(&format!("extensions:      {extensions}")),
            "--list-formats must advertise {format} extensions {extensions:?}, section was: {section}"
        );
    }

    // `--help-format` reads the same table.
    let (code, out, _) = run(&["--help-format", "yaml"], "");
    assert_eq!(code, 0);
    let text = String::from_utf8(out).expect("utf-8");
    assert!(
        text.contains("extensions:      yaml yml"),
        "--help-format yaml must print its extensions: {text}"
    );
}

/// A named `.jsonc` file detects as JSONC : comments and a trailing comma decode without a `--input-format` flag.
#[test]
fn jsonc_file_detects_by_extension() {
    let path = write_temp("jsonc", b"{\n  // compiler options\n  \"target\": \"ES2020\",\n}\n");
    let out = stdout_text(&["-c", ".", path.to_str().unwrap()], "");
    assert_eq!(out, "{\"target\":\"ES2020\"}\n");
    let _ = std::fs::remove_file(&path);
}

/// `--input-format jsonc` over stdin accepts the tsconfig corpus; the same bytes over the default strict json fail with
/// the JSONC hint naming the dialect.
#[test]
fn jsonc_input_format_and_the_strict_hint() {
    let bytes = "{\n  // comment\n  \"a\": 1,\n}\n";
    let (code, out, _) = run(&["-c", "--input-format", "jsonc", "."], bytes);
    assert_eq!(code, 0, "jsonc decode of a commented document must succeed");
    assert_eq!(out, b"{\"a\":1}\n");

    // The strict default rejects the same bytes with the jsonc hint.
    let (code, out, err) = run(&["-c", "."], bytes);
    assert_eq!(code, 5, "strict json must reject a comment");
    assert_eq!(out.len(), 0, "no bytes on a decode failure");
    let err = String::from_utf8_lossy(&err);
    assert!(
        err.contains("--input-format jsonc"),
        "the strict failure must hint jsonc: {err}"
    );
}

/// The `jsonc.trailing@1` dialect is the jsonc default and the `jsonc.default@1` dialect rejects a trailing comma.
#[test]
fn jsonc_dialects_differ_on_trailing_commas() {
    let bytes = "{\"a\": 1,}\n";
    let (code, out, _) = run(&["-c", "--input-format", "jsonc", "."], bytes);
    assert_eq!(code, 0, "the trailing dialect is the jsonc default");
    assert_eq!(out, b"{\"a\":1}\n");
    let (code, out, _) = run(
        &[
            "-c",
            "--input-format",
            "jsonc",
            "--input-dialect",
            "jsonc.default@1",
            ".",
        ],
        bytes,
    );
    assert_eq!(code, 5, "the strict-comma dialect rejects a trailing comma");
    assert_eq!(out.len(), 0);
}

/// A MULTI-LINE comment survives the round trip as bytes the same codec can re-read. The fact carries the block
/// comment's text, newlines and all; one `//` in front of the whole text left every continuation line outside the
/// comment, so a plain `.` over a file with a license header emitted a document this codec then rejected.
#[test]
fn a_multi_line_comment_round_trips_through_both_commented_dialects() {
    // Both owners of a leading comment: a MEMBER's, and the document ROOT's (a license header above the opening brace).
    // Two emission sites, one law.
    for bytes in ["{/* alpha\nbeta */ \"a\": 1}\n", "/* alpha\nbeta */\n{\"a\": 1}\n"] {
        for format in ["jsonc", "json5"] {
            let (code, out, err) = run(&["--input-format", format, "--output-format", format, "."], bytes);
            assert_eq!(code, 0, "{format} re-encode: {}", String::from_utf8_lossy(&err));
            let text = String::from_utf8(out.clone()).expect("utf-8");
            assert!(
                text.contains("// alpha") && text.contains("// beta"),
                "each comment line keeps its own marker: {text}"
            );
            // The whole point: the emitted bytes are readable by the same codec.
            let (code, reread, err) = run(
                &["-c", "--input-format", format, "."],
                &String::from_utf8(out).expect("utf-8"),
            );
            assert_eq!(
                code,
                0,
                "{format} must re-read its own output: {}",
                String::from_utf8_lossy(&err)
            );
            assert_eq!(reread, b"{\"a\":1}\n");
        }
    }
}

/// A count over a JSONC array with a `//` in the deferred span must not take the STRICT materializer. The leading
/// comment is trivia; the inner comment sits in `.a`, which a lazy whole-document decode defers.
#[test]
fn jsonc_count_survives_a_comment_in_a_deferred_array() {
    let (code, out, err) = run(
        &["-c", "--input-format", "jsonc", "[.a[] | .] | length"],
        "// c\n{\"a\":[1, // inner\n2, 3]}\n",
    );
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
    assert_eq!(out, b"3\n");
}

/// The commented encoder is recycled between the values of one stream, and its dialect arming belongs to the ITEM, not
/// to the recycled slot: every value after the first must still be written in the dialect that asked for it, never in
/// whatever the previous value left behind.
#[test]
fn every_value_of_a_jsonc_stream_is_written_in_the_jsonc_dialect() {
    let (code, out, err) = run(
        &["--input-format", "jsonc", "--output-format", "jsonc", ".[]"],
        "[{\"a\":1},{\"b\":2}]\n",
    );
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
    let text = String::from_utf8(out).expect("utf-8");
    assert_eq!(
        text.matches("1,").count() + text.matches("2,").count(),
        2,
        "both values keep the dialect's trailing comma: {text}"
    );
}

/// A named `.json5` file detects as JSON5 : the JSON5 grammar arms — unquoted keys, single quotes, comments — decode
/// without a `--input-format` flag.
#[test]
fn json5_file_detects_by_extension() {
    let path = write_temp("json5", b"{\n  // config\n  name: 'ada',\n  id: 0x10,\n}\n");
    let out = stdout_text(&["-c", ".", path.to_str().unwrap()], "");
    assert_eq!(out, "{\"name\":\"ada\",\"id\":16}\n");
    let _ = std::fs::remove_file(&path);
}

/// `--input-format json5` over stdin accepts the JSON5 grammar; the output re-encodes through the deterministic json5
/// profile.
#[test]
fn json5_input_format_decodes_the_grammar() {
    let bytes = "{\n  // lead\n  a: .5,\n  b: 'x',\n  c: 0xff,\n}\n";
    let (code, out, _) = run(&["-c", "--input-format", "json5", "."], bytes);
    assert_eq!(code, 0, "json5 decode of a json5 document must succeed");
    assert_eq!(out, b"{\"a\":0.5,\"b\":\"x\",\"c\":255}\n");
}
