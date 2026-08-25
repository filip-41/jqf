//! The CLI-flag surface: the output/exit-mode flags and the validation around them.
//!
//! Byte-identity against the compat corpus lives in `tools/jqf-cli-jq-compat.sh`. This file pins the parts the corpus
//! cannot express: the usage errors that reject invalid combinations, the exit-code law the `-e` flag exists to
//! deliver, and the colour switch law's acceptance corners (the palette parse, the raw-arm skip, the `-M`-wins
//! precedence).

use std::io::{Read as _, Write as _};
use std::process::{Command, Stdio};
use std::time::Duration;

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// Runs `jqf args…` with `stdin` as the input, returning (exit code, stdout, stderr).
fn run(args: &[&str], stdin: &str) -> (i32, Vec<u8>, Vec<u8>) {
    run_env(&[], args, stdin)
}

/// `run` with extra environment variables (`JQ_COLORS`, `NO_COLOR`).
fn run_env(env: &[(&str, &str)], args: &[&str], stdin: &str) -> (i32, Vec<u8>, Vec<u8>) {
    run_bytes(env, args, stdin.as_bytes())
}

/// `run` with raw byte stdin (the jqfb image is binary).
fn run_bytes(env: &[(&str, &str)], args: &[&str], stdin: &[u8]) -> (i32, Vec<u8>, Vec<u8>) {
    let mut command = Command::new(jqf_binary());
    command.env("JQF_NO_CONFIG", "1");
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in env {
        command.env(name, value);
    }
    let mut child = command.spawn().expect("jqf spawns");
    // A usage-error child exits WITHOUT reading stdin, closing the pipe mid-write; BrokenPipe is the expected race
    // there, not a test failure (surfaced by the 003 linux-amd64 emulated lane, where the child's exit reliably beats
    // the parent's write).
    if let Err(error) = child.stdin.take().expect("stdin is piped").write_all(stdin) {
        assert!(
            error.kind() == std::io::ErrorKind::BrokenPipe,
            "input writes to jqf's stdin: {error}"
        );
    }
    let output = child.wait_with_output().expect("jqf runs to completion");
    (output.status.code().unwrap_or(-1), output.stdout, output.stderr)
}

fn stdout(args: &[&str], stdin: &str) -> String {
    let (code, out, _) = run(args, stdin);
    assert_eq!(code, 0, "expected success for {args:?}, got {code}");
    String::from_utf8(out).expect("stdout is UTF-8")
}

/// Strips every ANSI SGR span (`ESC[…m`), leaving the decided bytes.
fn strip_ansi(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            i += 2;
            while i < bytes.len() && bytes[i] != b'm' {
                i += 1;
            }
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// The colour switch law : `-M` forces monochrome and always wins; `-C` forces colour ON even when piped; colour is a
/// RENDERING — stripping the spans recovers the decided bytes exactly.
#[test]
fn color_switches_follow_the_decision_law() {
    let plain = stdout(&["-c", "."], "{\"a\":1}");
    // `-M` and its long spelling: monochrome is the piped default anyway, so the bytes are exactly the plain run's.
    for flag in ["-M", "--monochrome-output"] {
        let flagged = stdout(&["-c", flag, "."], "{\"a\":1}");
        assert_eq!(flagged, plain, "flag {flag} must force monochrome");
    }
    // `-C` forces colour on even when piped (jq's law): the output carries ANSI spans, and stripping them recovers the
    // plain bytes exactly — the strip identity that says colour cannot change the byte stream.
    let (code, coloured, _) = run(&["-c", "-C", "."], "{\"a\":1}");
    assert_eq!(code, 0);
    assert!(
        coloured.windows(2).any(|w| w == [0x1b, b'[']),
        "-C output must carry ANSI spans"
    );
    assert_eq!(strip_ansi(&coloured), plain.as_bytes());
    // `-M` wins over `-C` in either order, and in a combined spelling (jq applies `-M` last; `-C -M -C` and `-M -C -M`
    // are monochrome).
    for args in [
        &["-c", "-M", "-C", "."][..],
        &["-c", "-C", "-M", "."][..],
        &["-c", "-CM", "."][..],
        &["-c", "-MC", "."][..],
    ] {
        assert_eq!(stdout(args, "{\"a\":1}"), plain, "{args:?} must be monochrome");
    }
}

/// render.terminal@1's tree shape colours under the same decision law: `-C` inserts spans over the frame's tokens,
/// stripping recovers the plain frame exactly, and `-M` stays monochrome. The plain and table shapes have no lexical
/// token law and stay monochrome even under `-C`.
#[test]
fn colour_renders_terminal_tree_frames() {
    let args = ["--output-format", "render", "--output-dialect", "render.terminal@1"];
    let plain = stdout(&args, "{\"a\":1,\"s\":\"x\"}");
    let (code, coloured, _) = run(&[&args[..], &["-C"]].concat(), "{\"a\":1,\"s\":\"x\"}");
    assert_eq!(code, 0);
    assert!(
        coloured.windows(2).any(|w| w == [0x1b, b'[']),
        "-C terminal output must carry ANSI spans"
    );
    assert_eq!(strip_ansi(&coloured), plain.as_bytes());
    // A custom palette field applies (slot 7 = keys).
    let (_, keyed, _) = run_env(
        &[("JQ_COLORS", "1;31:2;32:3;33:4;34:5;35:6;36:7;37:8;38")],
        &[&args[..], &["-C"]].concat(),
        "{\"a\":1}",
    );
    assert!(
        keyed.windows(b"\x1b[8;38m$".len()).any(|w| w == b"\x1b[8;38m$"),
        "the path must take the key colour"
    );
    // The other terminal shapes stay monochrome under `-C`.
    for shape in ["plain", "table"] {
        let shape_args = [
            "--output-format",
            "render",
            "--output-dialect",
            "render.terminal@1",
            "--render-shape",
            shape,
        ];
        let (code, out, _) = run(
            &[&shape_args[..], &["-C"]].concat(),
            if shape == "table" { "{\"a\":1}" } else { "3" },
        );
        assert_eq!(code, 0);
        assert!(
            !out.windows(2).any(|w| w == [0x1b, b'[']),
            "the {shape} shape has no token law and must stay monochrome"
        );
    }
}

/// The `-r` raw arm under colour (jq's law): a ROOT text item prints its own bytes with NO colour — even a string that
/// looks like a JSON literal — while a raw-printed non-string still colours.
#[test]
fn colour_raw_arm_skip() {
    // The string "null" under -r -C: jq prints the bare text, uncoloured.
    let (code, out, _) = run(&["-r", "-C", "."], "\"null\"");
    assert_eq!(code, 0);
    assert_eq!(out, b"null\n", "a raw root string is never coloured");
    // A raw-printed NUMBER is still a JSON rendering and colours.
    let (code, out, _) = run(&["-r", "-C", ".[0]"], "[1]");
    assert_eq!(code, 0);
    assert!(out.windows(2).any(|w| w == [0x1b, b'[']));
    assert_eq!(strip_ansi(&out), b"1\n");
}

/// The `JQ_COLORS` palette law: the eight fields apply, and a malformed value falls back to the defaults with jq's
/// exact stderr message — printed even though the run is monochrome (jq's unconditional message law).
#[test]
fn jq_colors_palette_and_failure() {
    let (code, out, _) = run_env(
        &[("JQ_COLORS", "1;31:2;32:3;33:4;34:5;35:6;36:7;37:8;38")],
        &["-c", "-C", "."],
        "{\"a\":1}",
    );
    assert_eq!(code, 0);
    assert!(
        out.starts_with(b"\x1b[7;37m{"),
        "the object colour must be the 7th field"
    );
    assert!(
        out.windows(b"\x1b[8;38m\"a\"\x1b[0m".len())
            .any(|w| w == b"\x1b[8;38m\"a\"\x1b[0m"),
        "the key colour must be the 8th field"
    );
    // A malformed palette: defaults render, jq's exact message on stderr, exit 0 — and the message prints even with -M
    // (jq's law).
    for args in [&["-c", "-C", "."][..], &["-c", "-M", "."][..]] {
        let (code, out, err) = run_env(&[("JQ_COLORS", "notacolor")], args, "{\"a\":1}");
        assert_eq!(code, 0);
        assert_eq!(err, b"Failed to set $JQ_COLORS\n", "{args:?}: jq's exact message");
        assert_eq!(strip_ansi(&out), b"{\"a\":1}\n", "defaults render the decided bytes");
    }
}

#[test]
fn combined_short_flags_expand() {
    // `-eS`, `-cja`, and friends expand into their single-letter flags.
    assert_eq!(stdout(&["-S", "-c", "."], "{\"b\":1,\"a\":2}"), "{\"a\":2,\"b\":1}\n");
    assert_eq!(stdout(&["-Sc", "."], "{\"b\":1,\"a\":2}"), "{\"a\":2,\"b\":1}\n");
    // `-ja`: a root string prints QUOTED with non-ASCII escaped, and `-j`'s no-newline law applies to the quoted form
    // too (jq's ASCII arm).
    assert_eq!(stdout(&["-ja", "."], "\"héllo\""), "\"h\\u00e9llo\"");
    // `-j` implies raw output: a root string prints raw, no trailing newline.
    assert_eq!(stdout(&["-j", "."], "\"hi\""), "hi");
    // A root object is not raw; `-j` only drops the item newline.
    assert_eq!(stdout(&["-j", "-c", "."], "{\"a\":1}"), "{\"a\":1}");
}

#[test]
fn bare_double_dash_ends_option_parsing() {
    // jq's `--`: everything after it is positional — the program or an input file, never a flag — so `jqf -- "$filter"`
    // survives a filter beginning with `-`. Byte identity against is pinned by the corpus's `flags` rows; this file
    // pins the parsing law itself. A `-`-leading filter that would otherwise be read as an unknown option (single dash
    // + letter) or an unknown combined flag runs as a program.
    assert_eq!(stdout(&["--", "-.a"], "{\"a\":5}"), "-5\n");
    assert_eq!(stdout(&["--", "-length"], "5"), "-5\n");
    assert_eq!(stdout(&["-n", "--", "-5"], ""), "-5\n");
    // `-.a + 1` would expand as a combined flag without the marker.
    assert_eq!(stdout(&["--", "-.a + 1"], "{\"a\":3}"), "-2\n");
    // A second `--` after the first is positional too, exactly as jq treats it: it becomes the program and fails to
    // parse, not an option error.
    let (code, _, _) = run(&["-n", "--", "--", "1"], "");
    assert_ne!(code, 2, "a second -- is not an unknown-option rejection");
    // Options parsed before the marker still stand.
    assert_eq!(stdout(&["-c", "-n", "--", ".a + 1"], ""), "1\n");
}

#[test]
fn format_flags_require_json_output() {
    for flag in ["-S", "-a", "-j"] {
        for output in ["toml", "csv", "cbor", "yaml"] {
            let (code, _, stderr) = run(&[flag, "--output-format", output, "."], "");
            assert_eq!(code, 2, "{flag} with {output} output must be a usage error");
            assert!(
                String::from_utf8_lossy(&stderr).contains("apply to JSON-family output only"),
                "unexpected message for {flag} {output}: {}",
                String::from_utf8_lossy(&stderr)
            );
        }
    }
}

#[test]
fn raw_output0_is_rejected_on_record_routes() {
    // The record route's terminator is codec-owned (NDJSON/CSV append their line terminator inside the encoder's own
    // staging buffer), so `--raw-output0`'s NUL item terminator cannot be honored there — half-applying it would leave
    // the LF in place and silently diverge from jq's NUL. The combination is a usage error before a byte is read, in
    // both `-r --raw-output0` and bare `--raw-output0` (which implies `-r`) spellings.
    for args in [
        vec!["--input-format", "ndjson", "-r", "--raw-output0", "."],
        vec!["--input-format", "ndjson", "--raw-output0", "."],
        vec!["--input-format", "csv", "-r", "--raw-output0", "."],
        vec!["--input-format", "tsv", "--raw-output0", "."],
    ] {
        let (code, stdout, stderr) = run(&args, "\"x\"\n\"y\"\n");
        assert_eq!(code, 2, "{args:?} must be a usage error");
        assert_eq!(stdout.len(), 0, "{args:?} must fail before publishing bytes");
        assert!(
            String::from_utf8_lossy(&stderr).contains("--raw-output0 cannot be combined with a record input"),
            "unexpected message for {args:?}: {}",
            String::from_utf8_lossy(&stderr)
        );
    }
    // The adjacent-value path keeps jq's law: the NUL terminator is honored.
    assert_eq!(stdout(&["-r", "--raw-output0", "."], "\"x\"\n\"y\"\n"), "x\u{0}y\u{0}");
}

#[test]
fn exit_status_reads_the_last_value() {
    // jq's `-e` law: 0 truthy, 1 false/null, 4 no output; a runtime error keeps exit 5. `0` is truthy (only false/null
    // are falsy).
    assert_eq!(run(&["-e", ".a > 3"], "{\"a\":5}").0, 0);
    assert_eq!(run(&["-e", ".a > 3"], "{\"a\":2}").0, 1);
    assert_eq!(run(&["-e", "empty"], "{}").0, 4);
    assert_eq!(run(&["-e", "."], "null").0, 1);
    assert_eq!(run(&["-e", "."], "0").0, 0);
    assert_eq!(run(&["-e", "error(\"boom\")"], "{}").0, 5);
    // The value, not the bytes: the string "false" is truthy.
    assert_eq!(run(&["-re", "."], "false").0, 1);
    assert_eq!(run(&["-re", "."], "\"false\"").0, 0);
    // The last published item across a multi-value stream wins.
    assert_eq!(run(&["-e", "."], "1\nfalse\n").0, 1);
    assert_eq!(run(&["-e", "."], "false\n1\n").0, 0);
}

#[test]
fn exit_status_rejects_document_lanes() {
    let (code, _, _) = run(&["-e", "--edit", "."], "{}");
    assert_eq!(code, 2, "-e with --edit must be a usage error");
}

#[test]
fn exit_status_plans_serial_on_the_record_route() {
    // `-e` reads the LAST OUTPUT VALUE, which a morsel worker never delivers (the relay publishes bytes only), so a
    // record request with `-e` plans SERIAL with the printable PlanDecision:ExitStatus — the AGENTS.md "-e plans
    // SERIAL" law, previously pinned only by the merge-tier capability gate. The plan line under `--diagnostics` is
    // `jqf: plan: requested=… decision=… workers=…`.
    let (code, out, stderr) = run(&["--input-format", "ndjson", "--diagnostics", "-e", "."], "true\n");
    assert_eq!(code, 0, "-e over a truthy last record exits 0");
    assert_eq!(out, b"true\n", "the record still publishes its value");
    let text = String::from_utf8_lossy(&stderr);
    let line = text
        .lines()
        .find(|line| line.starts_with("jqf: plan:"))
        .unwrap_or_else(|| panic!("no plan line under --diagnostics: {text}"));
    assert!(
        line.contains("decision=exit-status"),
        "the plan must name the -e decline: {line}"
    );
    assert!(
        line.contains("workers=0"),
        "an exit-status request must plan serial (zero workers): {line}"
    );
}

#[test]
fn unbuffered_plans_serial_on_the_record_route() {
    // `--unbuffered` flushes per ITEM, but a morsel worker's finish_item is a no-op and the relay publishes per MORSEL
    // — an eligible strict NDJSON request with the flag would stream at morsel cadence under a flag that promises item
    // cadence. The record planner declines it exactly as the value lane does: serial with the printable
    // PlanDecision:Unbuffered (`decision=unbuffered`).
    let (code, out, stderr) = run(
        &[
            "--input-format",
            "ndjson",
            "--diagnostics",
            "--unbuffered",
            "--workers",
            "4",
            ".",
        ],
        "{\"a\":1}\n{\"a\":2}\n",
    );
    assert_eq!(code, 0, "the records still serve");
    assert_eq!(out, b"{\n  \"a\": 1\n}\n{\n  \"a\": 2\n}\n", "serial's bytes");
    let text = String::from_utf8_lossy(&stderr);
    let line = text
        .lines()
        .find(|line| line.starts_with("jqf: plan:"))
        .unwrap_or_else(|| panic!("no plan line under --diagnostics: {text}"));
    assert!(
        line.contains("decision=unbuffered"),
        "the plan must name the --unbuffered decline: {line}"
    );
    assert!(
        line.contains("workers=0"),
        "an unbuffered request must plan serial (zero workers): {line}"
    );
}

#[test]
fn recovering_framing_plans_serial_on_the_record_route() {
    // Only a STRICT framing profile is morsel-eligible: a recovering framer reports ordered issues carrying absolute
    // offsets, which a worker holding one byte range cannot render. `--seq` without an explicit input selection engages
    // the flag-scoped RECOVERING json-seq route, and it must decline with the same printable `input-ineligible`
    // decision recovering NDJSON uses — even under an explicit width that would otherwise engage the coordinator.
    // Before the gate covered json-seq, this request planned parallel and stayed byte-correct only through the
    // unclean-morsel yield.
    for (args, input) in [
        (
            vec!["--seq", "--diagnostics", "--workers", "4", "."],
            "\x1e{\"a\":1}\n\x1e{\"a\":2}\n",
        ),
        (
            vec![
                "--input-format",
                "ndjson",
                "--input-dialect",
                "ndjson.recovering@1",
                "--diagnostics",
                "--workers",
                "4",
                ".",
            ],
            "{\"a\":1}\n{\"a\":2}\n",
        ),
    ] {
        let (code, _, stderr) = run(&args, input);
        assert_eq!(code, 0, "{args:?} serves its records");
        let text = String::from_utf8_lossy(&stderr);
        let line = text
            .lines()
            .find(|line| line.starts_with("jqf: plan:"))
            .unwrap_or_else(|| panic!("no plan line under --diagnostics: {text}"));
        assert!(
            line.contains("decision=input-ineligible"),
            "a recovering stream must decline the morsel lane by name: {line}"
        );
        assert!(
            line.contains("workers=0"),
            "a recovering stream must plan serial: {line}"
        );
    }
    // The contrast arm: STRICT json-seq passes the framing gate. The input is far below any useful morsel split, so the
    // planner lands on single-morsel — the decision proves only that eligibility held; a regression back to declining
    // strict seq shows up as input-ineligible.
    let (code, _, stderr) = run(
        &["--input-format", "json-seq", "--diagnostics", "--workers", "4", "."],
        "\x1e{\"a\":1}\n\x1e{\"a\":2}\n",
    );
    assert_eq!(code, 0);
    let text = String::from_utf8_lossy(&stderr);
    let line = text
        .lines()
        .find(|line| line.starts_with("jqf: plan:"))
        .unwrap_or_else(|| panic!("no plan line under --diagnostics: {text}"));
    assert!(
        !line.contains("input-ineligible"),
        "strict json-seq is morsel-eligible and must not decline: {line}"
    );
}

#[test]
fn stream_rejects_the_document_lanes_it_cannot_serve() {
    // `--stream` rewrites the input into `[path, leaf]` events, so the document-subject lanes are usage errors, never
    // accepted-and-useless: `--edit --stream` used to run the edit once per event and error per record (`Cannot index
    // array with string` per event, exit 5), and `--diff --stream` silently produced NO diff (differing files, exit 0,
    // empty). Both are now rejected before a byte is read.
    let (code, _, stderr) = run(&["--stream", "--edit", ".a = 2"], "{}");
    assert_eq!(code, 2, "--edit with --stream must be a usage error");
    assert!(
        String::from_utf8_lossy(&stderr).contains("--edit cannot be combined"),
        "the --edit fence must name the pair, got {stderr:?}"
    );
    let (code, _, stderr) = run(&["--stream", "--diff", "a", "b", "."], "");
    assert_eq!(code, 2, "--diff with --stream must be a usage error");
    assert!(
        String::from_utf8_lossy(&stderr).contains("--diff cannot be combined"),
        "the --diff fence must name the pair, got {stderr:?}"
    );
}

#[test]
fn diff_exit_code_gates_equality() {
    // The diff exit law: 0 when the two documents are semantically equal, 1 when they differ — the CI gate ("fail if
    // these drifted") without parsing the output. The fixed program emits ONE array of change records; an empty array
    // is the equality verdict. Usage and runtime failures keep their own classes.
    let equal_a = write_temp("{\"a\":1,\"b\":[1,2]}\n", "diff-eq-a.json");
    let equal_b = write_temp("{\"a\":1,\"b\":[1,2]}\n", "diff-eq-b.json");
    let drifted = write_temp("{\"a\":2,\"b\":[1,2]}\n", "diff-drift.json");
    let (code, out, err) = run(&["--diff", equal_a.to_str().unwrap(), equal_b.to_str().unwrap()], "");
    assert_eq!(code, 0, "equal documents exit 0: {err:?}");
    assert_eq!(out, b"[]\n", "equal documents emit the empty record array");
    let (code, out, _) = run(&["--diff", equal_a.to_str().unwrap(), drifted.to_str().unwrap()], "");
    assert_eq!(code, 1, "differing documents exit 1");
    assert!(
        out != b"[]\n" && !out.is_empty(),
        "differing documents emit change records: {out:?}"
    );
}

#[test]
fn diff_yaml_side_compares_the_first_document_of_a_stream() {
    // syntax165 T6: a YAML side is a `---`-separated document STREAM, so a multi-document YAML file no longer refuses
    // the diff lane with "expected exactly one document" — each side compares its FIRST unit, the same
    // one-item-per-unit law every other YAML route serves.
    let old = write_temp("a: 1\n---\nb: 2\n", "diff-yam-old.yaml");
    let new = write_temp("a: 2\n---\nb: 2\n", "diff-yam-new.yaml");
    let (code, out, err) = run(
        &[
            "--input-format",
            "yaml",
            "--diff",
            old.to_str().unwrap(),
            new.to_str().unwrap(),
        ],
        "",
    );
    assert_eq!(code, 1, "first documents differ; stderr={err:?}");
    let out = String::from_utf8(out).expect("utf8");
    assert!(
        out.contains("\"a\"") && !out.contains("\"b\""),
        "the diff names only the first unit's change: {out}"
    );
}

#[test]
fn single_document_output_formats_refuse_a_second_document() {
    // TOML/XML output has no multi-document framing: a second document's blank-line-separated bytes no parser reads
    // back ( `--output-format toml` over `{"a":1} {"a":2}` emitted `a = 1\n\na = 2`, invalid TOML — a duplicate key;
    // disjoint keys silently merged two documents into one). The FIRST document stays published (the standing
    // prefix-keep law); the second is refused with a message naming the shape. Multi-document output formats are
    // untouched: YAML still emits its `---` stream and NDJSON its records.
    let input = "{\"a\":1}\n{\"a\":2}\n";
    let (code, out, stderr) = run(&["--output-format", "toml", "."], input);
    assert_eq!(code, 5, "a second TOML document must refuse");
    // The TOML encoder's own final LF plus the facade suffix (the existing double-newline spelling) — the first
    // document stays published.
    assert_eq!(out, b"a = 1\n\n", "the first document stays published");
    assert!(
        String::from_utf8_lossy(&stderr).contains("one document per run"),
        "the refusal must name the shape, got {stderr:?}"
    );
    let (code, _, _) = run(&["--output-format", "xml", "."], input);
    assert_eq!(code, 5, "a second XML document must refuse");
    // YAML owns a real multi-document framing; NDJSON its records.
    assert_eq!(stdout(&["--output-format", "yaml", "."], input), "a: 1\n---\na: 2\n");
    assert_eq!(
        stdout(&["--output-format", "ndjson", "."], input),
        "{\"a\":1}\n{\"a\":2}\n"
    );
}

#[test]
fn positional_args_bind_arounds() {
    assert_eq!(
        stdout(&["-n", "$ARGS.positional", "--args", "a", "b", "c"], ""),
        "[\n  \"a\",\n  \"b\",\n  \"c\"\n]\n"
    );
    // `$ARGS` is always bound, and a user `--arg ARGS` cannot shadow it.
    assert_eq!(stdout(&["-n", "$ARGS.positional"], ""), "[]\n");
    assert_eq!(stdout(&["-n", "$ARGS.named.ARGS", "--arg", "ARGS", "x"], ""), "\"x\"\n");
}

#[test]
fn stream_wires_tostream_to_stdin() {
    let input = "{\"a\":1,\"b\":[2,3],\"c\":{\"d\":4}}";
    let stream = stdout(&["--stream", "-c", "."], input);
    assert_eq!(
        stream,
        "[[\"a\"],1]\n[[\"b\",0],2]\n[[\"b\",1],3]\n[[\"b\",1]]\n[[\"c\",\"d\"],4]\n[[\"c\",\"d\"]]\n[[\"c\"]]\n"
    );
    // With -s, the stream items are collected into ONE array before the program runs.
    let slurped = stdout(&["-s", "--stream", "-c", "."], "{\"a\":1} {\"b\":2}");
    assert_eq!(slurped, "[[[\"a\"],1],[[\"a\"]],[[\"b\"],2],[[\"b\"]]]\n");
    // -n and -R change the input model and take precedence over --stream.
    assert_eq!(stdout(&["-n", "--stream", "-c", "."], "x"), "null\n");
    assert_eq!(stdout(&["-R", "--stream", "-c", "."], "hello\n"), "\"hello\"\n");
}

#[test]
fn stream_errors_implies_stream_and_reports_refusals_as_events() {
    // `--stream-errors`: it IMPLIES `--stream` (the help says so; there is NO diagnostic for using it alone) and
    // reports parse refusals as `[message, path]` events with earlier events standing and parsing resuming at the next
    // line. Exit 0.
    let events = stdout(&["--stream-errors", "-c", "."], "{\"a\":1}\n{bad}\n{\"c\":2}");
    assert_eq!(
        events,
        "[[\"a\"],1]\n[[\"a\"]]\n[\"Invalid numeric literal at line 2, column 5\",[null]]\n[[\"c\"],2]\n[[\"c\"]]\n"
    );
    // Alone, it still streams (the coupling); the event shapes are the plain `--stream` route's.
    let alone = stdout(&["--stream-errors", "-c", "."], "{\"a\":1}");
    assert_eq!(alone, "[[\"a\"],1]\n[[\"a\"]]\n");
    // With -s the events (error events included) are collected into ONE array, exactly as jq slurps its stream.
    let slurped = stdout(&["-s", "--stream-errors", "-c", "."], "{bad}\n{\"c\":2}");
    assert_eq!(
        slurped,
        "[[\"Invalid numeric literal at line 1, column 5\",[null]],[[\"c\"],2],[[\"c\"]]]\n"
    );
    // Under plain `--stream` the same refusal is terminal (exit 5) with the earlier events' bytes already published.
    let (code, out, _err) = run(&["--stream", "-c", "."], "{\"a\":1}\n{bad}");
    assert_eq!(String::from_utf8(out).unwrap(), "[[\"a\"],1]\n[[\"a\"]]\n");
    assert_eq!(code, 5);
}

/// Reads one newline-terminated record from the follow child's stdout with a hard deadline (the follow poll cadence is
/// 100 ms, so a rotation's drained tail must arrive well inside the deadline).
fn read_follow_line(reader: &mut impl std::io::Read) -> Vec<u8> {
    let mut line = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let mut byte = [0u8; 1];
        match reader.read(&mut byte) {
            Ok(0) => panic!("follow stdout closed before the expected record"),
            Ok(_) => {
                line.push(byte[0]);
                if byte[0] == b'\n' {
                    return line;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => panic!("follow stdout read: {error}"),
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for a follow record; got {line:?}"
        );
    }
}

#[test]
fn follow_rotates_with_a_renamed_and_recreated_file() {
    // (D3, tail -F semantics): default logrotate renames the live file and creates a fresh one. The follower must
    // detect the inode switch on its poll, deliver the records written to the renamed file (the tail drain), and follow
    // the new file from byte 0 — records before, across, and after rotation all delivered. The truncation path (shrink
    // -> Restarted) is untouched and separately pinned.
    let dir = std::env::temp_dir().join(format!(
        "jqf-follow-rotate-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("log.ndjson");
    std::fs::write(&path, "{\"v\":1}\n").expect("seed record");
    let mut child = Command::new(jqf_binary());
    child.env("JQF_NO_CONFIG", "1");
    child
        .args(["--follow", "-c", ".v", path.to_str().expect("utf8 path")])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = child.spawn().expect("follow spawns");
    let mut stdout = child.stdout.take().expect("follow stdout");
    // Record 1 from the original file.
    assert_eq!(read_follow_line(&mut stdout), b"1\n");
    // Rotate: rename the live file away, write a record to the RENAMED file (the tail-drain law — a writer that flushed
    // before the rotation must still be delivered), then create the fresh file with a record.
    let rotated = dir.join("log.ndjson.1");
    std::fs::rename(&path, &rotated).expect("rename");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&rotated)
        .expect("open rotated")
        .write_all(b"{\"v\":2}\n")
        .expect("old tail");
    std::fs::write(&path, "{\"v\":3}\n").expect("new file");
    // The renamed file's tail, then the new file's record — in order.
    assert_eq!(read_follow_line(&mut stdout), b"2\n");
    assert_eq!(read_follow_line(&mut stdout), b"3\n");
    // The rotation is announced on stderr as an advisory. Stop the daemon and drain its stderr (the advisory may be in
    // the buffer already).
    child.kill().expect("kill");
    let _ = child.wait();
    let mut follow_stderr = String::new();
    child
        .stderr
        .take()
        .expect("follow stderr")
        .read_to_string(&mut follow_stderr)
        .expect("stderr reads");
    // The rotation advisory is pinned HARD: the Rotated arm emits it and flushes the buffered stderr channel, so a
    // piped consumer sees the line promptly (the live-window arm pairs its own emission with a flush, and the idle poll
    // drains the buffer). The rotation itself is also pinned by the e2e script.
    assert!(
        follow_stderr.contains("file rotated; reopened"),
        "rotation advisory missing from stderr: {follow_stderr:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn follow_pipe_is_byte_identical_to_the_whole_input_record_route() {
    // `--follow` over a complete piped NDJSON stream must publish exactly what the whole-input record route publishes:
    // a pipe is read to EOF and its trailing record finalized, which is byte-identical for a complete stream.
    let input = "{\"v\":1}\n{\"v\":2}\n{\"v\":3}\n";
    let followed = stdout(&["--follow", "-c", ".v"], input);
    let static_run = stdout(
        &[
            "--input-format",
            "ndjson",
            "--input-dialect",
            "ndjson.recovering@1",
            "-c",
            ".v",
        ],
        input,
    );
    assert_eq!(followed, static_run);
    assert_eq!(followed, "1\n2\n3\n");
}

#[test]
fn follow_json_seq_output_is_byte_identical_to_the_whole_input_record_route() {
    // JsonSeq joined the record route's output set at, and the follow route IS the record route fed incrementally, so
    // `--follow --output-format json-seq` must serve the same RS-prefixed bytes the whole-input record route publishes.
    // Follow's output set used to be a hand-written copy that silently omitted json-seq (finding 11); the shared
    // `require_record_capable_output` check keeps them one set.
    let input = "{\"v\":1}\n{\"v\":2}\n{\"v\":3}\n";
    let followed = stdout(&["--follow", "--output-format", "json-seq", "."], input);
    let whole = stdout(
        &[
            "--input-format",
            "ndjson",
            "--input-dialect",
            "ndjson.recovering@1",
            "--output-format",
            "json-seq",
            ".",
        ],
        input,
    );
    assert_eq!(followed, whole);
    // The json-seq framing law: one RS-prefixed, LF-terminated item per record (jq's `--seq` output pretty-prints by
    // default).
    assert_eq!(
        followed,
        "\u{1e}{\n  \"v\": 1\n}\n\u{1e}{\n  \"v\": 2\n}\n\u{1e}{\n  \"v\": 3\n}\n"
    );
}

#[test]
fn follow_pipe_finalizes_a_truncated_tail_at_eof() {
    // A truncated trailing record is held while the stream lives and finalized at EOF through the recovering dialect's
    // own law: an incomplete value is an ordered error issue, records before it stay published, and the exit class is
    // forced.
    let (code, out, err) = run(&["--follow", ".v"], "{\"v\":1}\n{\"v\":2");
    assert_eq!(code, 5, "truncated tail forces the runtime exit class");
    assert_eq!(String::from_utf8(out).unwrap(), "1\n");
    assert!(
        String::from_utf8_lossy(&err).contains("record error"),
        "the truncated tail must surface an ordered issue"
    );
}

#[test]
fn follow_records_stream_as_they_finish_and_keeps_last_value_law() {
    // A per-record value error does not kill the live stream; the LAST record decides the exit class, exactly as the
    // whole-input record route keeps.
    let (code, out, err) = run(&["--follow", ".a[0]"], "5\n{\"a\":[1]}\n");
    assert_eq!(code, 0, "a clean true last record wins");
    assert_eq!(String::from_utf8(out).unwrap(), "1\n");
    assert!(String::from_utf8_lossy(&err).contains("at <stdin>:1"));

    let (code, _, _) = run(&["--follow", ".a[0]"], "{\"a\":[1]}\n5\n");
    assert_eq!(code, 5, "a failed true last record forces the runtime class");
}

#[test]
fn follow_an_over_ceiling_cycle_reports_and_the_tail_continues() {
    // D3 : a live tail whose next record over-allocates crosses the RSS ceiling; that CYCLE gets a per-cycle diagnostic
    // and the tail goes on — one poison record must never kill the live tail. The good record is written only after the
    // refusal diagnostic lands (so it arrives in a LATER cycle) and is still served, exactly as the per-value
    // poison-record law promises.
    let mut child = Command::new(jqf_binary());
    child.env("JQF_NO_CONFIG", "1");
    child
        .args(["--follow", "-c", "--max-rss", "24M", "[range(.)]"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = child.spawn().expect("follow spawns");
    let mut stdin = child.stdin.take().expect("follow stdin");
    let mut stdout = child.stdout.take().expect("follow stdout");
    let mut stderr = child.stderr.take().expect("follow stderr");
    // Drain stderr on a thread (a full pipe would block the tail); the refusal diagnostic is the signal that the poison
    // cycle ran and the tail is waiting for the next record.
    let stderr_seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    {
        let seen = std::sync::Arc::clone(&stderr_seen);
        std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            loop {
                match stderr.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => seen.lock().expect("stderr lock").extend_from_slice(&buffer[..n]),
                }
            }
        });
    }
    // The poison record: materializes ~20M integers, far over the ceiling.
    stdin.write_all(b"20000000\n").expect("write poison");
    // The refusal is a governor-sampling event, not a wall-clock constant: under load it can arrive late, so the
    // deadline is generous (V1-F2, — the old fixed 10 s was fragile). The real failure mode is a child that DIED
    // without reporting; fail fast on that with whatever stderr arrived.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        {
            let seen = stderr_seen.lock().expect("stderr lock");
            if String::from_utf8_lossy(&seen).contains("physical memory ceiling exceeded") {
                break;
            }
        }
        if let Some(status) = child.try_wait().expect("try_wait") {
            let seen = stderr_seen.lock().expect("stderr lock");
            panic!(
                "the follow child exited {status} without reporting the refusal: {:?}",
                String::from_utf8_lossy(&seen)
            );
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the per-cycle refusal never arrived: {:?}",
            String::from_utf8_lossy(&stderr_seen.lock().expect("stderr lock"))
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // The good record: a LATER cycle, still served. EOF finalizes the tail.
    stdin.write_all(b"3\n").expect("write good");
    drop(stdin);
    let mut out = Vec::new();
    stdout.read_to_end(&mut out).expect("read stdout");
    assert_eq!(
        out, b"[0,1,2]\n",
        "the record after the refused cycle must still publish: {out:?}"
    );
    let status = child.wait().expect("follow waits");
    assert_eq!(status.code(), Some(0), "the tail must survive the over-ceiling cycle");
}

#[test]
fn follow_input_family_serves_the_live_window_shape() {
    // gate 3 — the acceptance that SHIPPED BROKEN: `--follow` with an input-family program runs the program ONCE over
    // the live stream with records served through `inputs`, so the rolling builtins' `foreach` state persists across
    // the whole tail. The per-record drive sees `inputs` empty (jq's own law), which is how the advertised `--follow
    // 'ewma(0.2; inputs|.ms)'` composition silently published nothing.
    let input = "{\"ms\":100}\n{\"ms\":200}\n{\"ms\":300}\n";
    let followed = stdout(&["--follow", "ewma(0.2; inputs|.ms)"], input);
    // The `-n` single-run answer over the same bytes: both drives share the stream-long window state.
    let single_run = stdout(&["-n", "ewma(0.2; inputs|.ms)"], input);
    assert_eq!(followed, single_run);
    assert_eq!(followed, "100\n120\n156\n");
    // The explicit `-n --follow` spelling is the same drive (the args-level `-n` refusal was narrowed to the non-input
    // programs, refused after compile instead).
    let explicit = stdout(&["-n", "--follow", "ewma(0.2; inputs|.ms)"], input);
    assert_eq!(explicit, single_run);
    // A bare `inputs` tail emits every record, like `tail -f | jq -n 'inputs'`.
    let inputs_tail = stdout(&["--follow", "inputs"], "1\n2\n3\n");
    assert_eq!(inputs_tail, "1\n2\n3\n");
    // The non-input shapes the live-window arm cannot serve stay usage errors: `-n` over a program that never reads the
    // input (it would run once over null and end the tail) and the input family over a CSV tail (CSV bytes are framed
    // records, not adjacent JSON).
    let (code, _, stderr) = run(&["--follow", "-n", "."], "");
    assert_eq!(code, 2, "-n --follow needs an input-family program");
    assert!(!stderr.is_empty());
    let (code, _, stderr) = run(&["--follow", "--input-format", "csv", "inputs"], "");
    assert_eq!(code, 2, "input family over a CSV tail is a usage error");
    assert!(!stderr.is_empty());
}

#[test]
fn seed_makes_a_repeated_run_byte_identical() {
    // `--seed N` (a jqf extension beyond jq) primes the rand family's shared draw state: two runs with the same seed
    // answer byte-identically, and a different seed answers differently (2000 draws makes an accidental collision
    // astronomically unlikely).
    let program = "[limit(2000; repeat(randint(0; 1000000000)))] | tostring";
    let first = stdout(&["-n", "--seed", "11", program], "");
    let second = stdout(&["-n", "--seed", "11", program], "");
    assert_eq!(first, second, "same seed must answer byte-identically");
    let different = stdout(&["-n", "--seed", "12", program], "");
    assert_ne!(first, different, "a different seed must not answer identically");
}

#[test]
fn seed_leaves_rand_seed_argument_unaffected() {
    // `rand(seed)` is already deterministic from its OWN argument; `--seed` primes a SEPARATE draw state and must not
    // change `rand(seed)`'s answer.
    let unseeded = stdout(&["-n", "rand(7)"], "");
    let seeded = stdout(&["-n", "--seed", "99", "rand(7)"], "");
    assert_eq!(unseeded, seeded, "--seed must not affect rand(seed)");
}

#[test]
fn seed_may_only_be_given_once() {
    let (code, _, stderr) = run(&["--seed", "1", "--seed", "2", "."], "null");
    assert_eq!(code, 2, "a repeated --seed must be a usage error");
    assert!(String::from_utf8_lossy(&stderr).contains("--seed may only be given once"));
}

#[test]
fn seed_rejects_a_non_integer_value() {
    let (code, _, stderr) = run(&["--seed", "not-a-number", "."], "null");
    assert_eq!(code, 2, "a non-integer --seed must be a usage error");
    assert!(String::from_utf8_lossy(&stderr).contains("--seed value is not a valid integer"));
}

#[test]
fn raw_output0_terminates_items_with_nul_not_newline() {
    let (code, out, _) = run(&["--raw-output0", "."], "\"hello\"");
    assert_eq!(code, 0);
    assert_eq!(out, b"hello\0");
    // Multiple items: every item, not only the root string, is NUL-terminated.
    let (code, out, _) = run(&["-n", "--raw-output0", "1,2,3"], "");
    assert_eq!(code, 0);
    assert_eq!(out, b"1\x002\x003\0");
}

#[test]
fn raw_output0_overrides_join_output() {
    // jq's `--raw-output0` overrides `-j`'s empty terminator with NUL, regardless of which flag is given first.
    let (code, out, _) = run(&["-n", "-j", "--raw-output0", "\"a\",\"b\""], "");
    assert_eq!(code, 0);
    assert_eq!(out, b"a\0b\0");
    let (code, out, _) = run(&["-n", "--raw-output0", "-j", "\"a\",\"b\""], "");
    assert_eq!(code, 0);
    assert_eq!(out, b"a\0b\0");
}

#[test]
fn raw_output0_rejects_a_root_string_containing_nul() {
    // jq's own guard and wording (the token-span pass): a root string dumped raw that itself contains a literal NUL
    // byte cannot be told apart from the NUL terminator, so it is a runtime error rather than a silently corrupted
    // stream. `\u0000` in the JSON source decodes to a real NUL byte.
    let (code, out, stderr) = run(&["--raw-output0", "."], "\"ab\\u0000cd\"");
    assert_eq!(code, 5, "a NUL in the raw-dumped root string is a runtime error");
    assert!(out.is_empty(), "the offending item must not be published");
    assert!(
        String::from_utf8_lossy(&stderr).contains("Cannot dump a string containing NUL with --raw-output0 option"),
        "unexpected message: {}",
        String::from_utf8_lossy(&stderr)
    );
    // A plain -r (no --raw-output0) has no NUL terminator to collide with, so the same string passes straight through,
    // literal NUL byte and all.
    let (code, out, _) = run(&["-r", "."], "\"ab\\u0000cd\"");
    assert_eq!(code, 0);
    assert_eq!(out, b"ab\0cd\n");
}

#[test]
fn raw_output0_requires_json_output() {
    for output in ["ndjson", "toml", "csv", "cbor", "yaml"] {
        let (code, _, stderr) = run(&["--raw-output0", "--output-format", output, "."], "\"x\"");
        assert_eq!(code, 2, "--raw-output0 with {output} output must be a usage error");
        assert!(
            String::from_utf8_lossy(&stderr).contains("apply to JSON-family output only"),
            "unexpected message for {output}: {}",
            String::from_utf8_lossy(&stderr)
        );
    }
}

#[test]
fn version_flag_prints_jqfs_own_version_and_exits_zero() {
    for flag in ["-V", "--version"] {
        let (code, out, _) = run(&[flag], "");
        assert_eq!(code, 0, "{flag} must exit 0 without reading stdin");
        let text = String::from_utf8(out).expect("version is UTF-8");
        assert!(
            text.starts_with("jqf-"),
            "{flag} must print jqf's own name, not jq's: {text:?}"
        );
        assert!(text.ends_with('\n'));
    }
}

#[test]
fn record_route_rejects_document_shaped_output_formats() {
    // A record request frames the input into records and re-encodes each; document-shaped outputs (TOML, CBOR, YAML,
    // XML, …) have no record encoding, so the route refuses before a byte is read — exit 2 with the "not supported on a
    // record input" message (the merge-tier capability gate's cell was the only prior pin).
    let (code, _, stderr) = run(
        &["--input-format", "ndjson", "--output-format", "toml", "."],
        "{\"a\":1}\n",
    );
    assert_eq!(code, 2, "a document output format on a record input is a usage error");
    assert!(
        String::from_utf8_lossy(&stderr).contains("--output-format toml is not supported on a record input"),
        "the refusal must name the pair and the rule: {stderr:?}"
    );
}

#[test]
fn record_route_renders_one_frame_per_record() {
    // The render family serves the record route: each record renders to its own complete frame and the facade appends
    // the frame's final LF, so an NDJSON stream aggregates into per-record presentation end to end.
    let (code, out, stderr) = run(
        &[
            "--input-format",
            "ndjson",
            "--output-format",
            "render",
            "--output-dialect",
            "render.tree@1",
            ".",
        ],
        "{\"id\":1}\n{\"id\":2}\n",
    );
    assert_eq!(code, 0, "record render must serve: {stderr:?}");
    assert_eq!(
        String::from_utf8(out).expect("UTF-8"),
        "$ = object(1)\n  $[\"id\"]#0 = 1\n$ = object(1)\n  $[\"id\"]#0 = 2\n"
    );
}

#[test]
fn record_route_render_still_refuses_a_non_table_record_shape() {
    // Serving the route does not waive the renderer's own shape law: a CSV array record is not a table item, so the
    // table dialect refuses with its typed shape error while the tree dialect over the same records serves.
    let (code, _, _) = run(
        &[
            "--input-format",
            "csv",
            "--output-format",
            "render",
            "--output-dialect",
            "render.gfm-table@1",
            ".",
        ],
        "a,b\n1,2\n",
    );
    assert_eq!(code, 5);
}

#[test]
fn follow_rejects_models_it_cannot_serve() {
    // The document lanes, the input models, destinations, and non-record formats are all usage errors before any byte
    // is read. Each case pins the exact rejection message too, so a fence that stops failing (or starts saying
    // something else) fails here rather than silently.
    for (args, reason) in [
        (
            &["--follow", "-s", "."][..],
            "-R/-s read the input to completion and cannot be combined",
        ),
        (
            &["--follow", "-n", "."],
            "-n over --follow's live stream is served only for programs that pull \
             `inputs`/`input`",
        ),
        (
            &["--follow", "-R", "."],
            "-R/-s read the input to completion and cannot be combined",
        ),
        (&["--follow", "--edit", "."], "--follow cannot be combined with --edit"),
        (
            &["--follow", "--stream", "."],
            "--follow cannot be combined with --stream",
        ),
        (
            &["--follow", "--diff", "a", "b", "."],
            "--follow cannot be combined with --diff",
        ),
        //: CSV follow is LIFTED — the RFC 4180 quote-state walk is
        // the exact cut for a quoted-record format, so CSV is a served model, not a rejection. Pinned positively by
        // `follow_csv_follow` in tools/jqf-follow-e2e.py and below.
        (
            &["--follow", "--input-format", "toml", "."],
            "--follow requires a newline-framed record input (ndjson) or RFC 4180 CSV",
        ),
        //: cbor-seq is an ADJACENT-value input, not a record
        // route — the same refusal shape json-seq falls into.
        (
            &["--follow", "--input-format", "cbor-seq", "."],
            "--follow requires a newline-framed record input (ndjson) or RFC 4180 CSV",
        ),
        (
            &["--follow", "--input-format", "json", "."],
            "--follow requires a newline-framed record input (ndjson) or RFC 4180 CSV",
        ),
        // The HEADERED CSV dialect is the one CSV shape a live tail cannot serve: the header row is a whole-stream
        // fact, and the cycle drive re-opens the framer and the payload provider on every refill, so each cycle would
        // consume its own first record as a header and key the rest by that record's values. A seekable stdin redirect
        // reads whole and serves it.
        (
            &[
                "--follow",
                "--input-format",
                "csv",
                "--input-dialect",
                "csv.rfc4180-header@1",
                ".",
            ],
            "the headered CSV dialect (csv.rfc4180-header@1) is not served by --follow",
        ),
        // An atomic file destination has no run end to commit: `--follow` streams results as records arrive, so
        // `--output` and `--in-place` are refused with the same fence before a byte is read.
        (
            &["--follow", "--output", "out.json", "."],
            "--follow streams results as records arrive; it cannot write an \
             atomic file destination",
        ),
        (
            &["--follow", "--in-place", "."],
            "--follow streams results as records arrive; it cannot write an \
             atomic file destination",
        ),
    ] {
        let (code, _, stderr) = run(args, "");
        assert_eq!(code, 2, "--follow must reject {args:?} as a usage error");
        let text = String::from_utf8_lossy(&stderr);
        assert!(
            text.contains(reason),
            "--follow rejection must say why, expected {reason:?}: {text}"
        );
    }
    // The lifted CSV follow: a finite piped CSV stream is byte-identical to the whole-input CSV route over the same
    // bytes (the held-tail law), including a quoted field that spans a newline.
    let (follow_code, follow_out, _) = run(
        &["--follow", "--input-format", "csv", "-c", "."],
        "a,b\n\"x\ny\",2\n3,4\n",
    );
    let (whole_code, whole_out, _) = run(&["--input-format", "csv", "-c", "."], "a,b\n\"x\ny\",2\n3,4\n");
    assert_eq!(follow_code, whole_code, "CSV follow exit matches the whole-input route");
    assert_eq!(follow_out, whole_out, "CSV follow bytes match the whole-input route");
}

#[test]
fn diagnostics_name_the_platform_and_physical_core_count() {
    // The provenance line is the one diagnostics fact printed on EVERY request (single-document lanes print no plan
    // line), so it is where a docker lane names its environment. `platform=` must match the running binary's own
    // target, `pcores=` must be a positive integer, and the source token must be one of the two legal spellings.
    let (code, _, stderr) = run(&["--diagnostics", "."], "{\"a\":1}");
    assert_eq!(code, 0, "a single-document request succeeds");
    let text = String::from_utf8_lossy(&stderr);
    let line = text
        .lines()
        .find(|line| line.contains("build=") && line.contains("pcores="))
        .unwrap_or_else(|| panic!("no provenance line with pcores in: {text}"));
    assert!(
        line.contains(&format!("platform={}-{}", std::env::consts::ARCH, std::env::consts::OS)),
        "the provenance line must name the running platform: {line}"
    );
    let pcores = line
        .split_whitespace()
        .find_map(|token| token.strip_prefix("pcores="))
        .expect("the provenance line carries pcores=");
    assert!(
        pcores.parse::<usize>().is_ok_and(|count| count >= 1),
        "pcores= must be a positive integer: {pcores}"
    );
    assert!(
        line.contains("pcore_source=detected") || line.contains("pcore_source=assumed"),
        "the provenance line must say whether the count was detected or assumed: {line}"
    );
}

// --- The mismatch dial (/W3): --mismatch-policy --------------------

/// The dial's three positions over the same cell: lenient answers jq's value silently, warn adds the aggregated
/// per-cell report to stderr with jq's exit code, strict raises with exit 5.
#[test]
fn mismatch_policy_dial_over_a_missing_key() {
    let input = "{\"a\":1}";
    let (code, out, err) = run(&["-c", ".b"], input);
    assert_eq!(code, 0);
    assert_eq!(String::from_utf8(out).unwrap(), "null\n");
    assert!(!String::from_utf8_lossy(&err).contains("mismatch"), "{err:?}");

    let (code, out, err) = run(&["-c", "--mismatch-policy", "warn", ".b"], input);
    assert_eq!(code, 0, "warn keeps jq's exit code");
    assert_eq!(String::from_utf8(out).unwrap(), "null\n", "warn answers jq's value");
    let err = String::from_utf8_lossy(&err);
    assert!(err.contains("mismatch report: missing-object-key=1"), "{err}");

    let (code, out, err) = run(&["-c", "--mismatch-policy", "strict", ".b"], input);
    assert_eq!(code, 5, "strict raises with the runtime exit class");
    assert!(out.is_empty(), "strict publishes no bytes for the failing value");
    let err = String::from_utf8_lossy(&err);
    assert!(
        err.contains("mismatch under strict policy: missing-object-key"),
        "{err}"
    );
}

/// Every frozen cell raises under strict with exit 5.
#[test]
fn strict_raises_every_frozen_cell() {
    for (program, input) in [
        (".b", "{\"a\":1}"),
        (".[9]", "[1,2]"),
        (".a", "null"),
        (".[0]", "null"),
        (".[1:2]", "null"),
        ("null + 1", "null"),
        ("getpath([\"a\",\"b\"])", "{}"),
        ("1 < \"a\"", "null"),
        ("length", "null"),
        ("reverse", "null"),
    ] {
        let (code, out, err) = run(&["-c", "--mismatch-policy", "strict", program], input);
        assert_eq!(code, 5, "strict raises for {program}");
        assert!(out.is_empty(), "strict publishes nothing for {program}");
        assert!(
            String::from_utf8_lossy(&err).contains("mismatch under strict policy"),
            "{program}: {err:?}"
        );
    }
}

/// Intent suppression: a `//` around the site, a `?`-marked step, and a `try` body all fire no event, so strict answers
/// exactly what jq answers.
#[test]
fn strict_intent_suppression_matches_the_adopted_law() {
    for (program, input, expected_stdout) in [
        (".b // \"x\"", "{\"a\":1}", "\"x\"\n"),
        (".b?", "{\"a\":1}", "null\n"),
        ("try .b catch \"x\"", "{\"a\":1}", "null\n"),
        ("null + 1 // 99", "null", "1\n"),
        ("null | length // 99", "null", "0\n"),
    ] {
        let (code, out, err) = run(&["-c", "--mismatch-policy", "strict", program], input);
        assert_eq!(code, 0, "a suppressed cell never raises: {program}: {err:?}");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            expected_stdout,
            "strict answers jq's value: {program}"
        );
        assert!(!String::from_utf8_lossy(&err).contains("mismatch"), "{program}");
    }
}

/// 012 Part A: a folded computed bound is INDISTINGUISHABLE from the authored literal under the mismatch dial — the
/// slice-clamp cell fires for both — and a bound the fold declines keeps its pre-fold silence (no cell, jq's answer).
#[test]
fn a_folded_computed_bound_fires_the_slice_clamp_cell_like_an_authored_literal() {
    let input = "[0]";
    for program in [".[3:2]", ".[(1+2):2]", "1 as $x | .[$x:2]"] {
        let (code, out, err) = run(&["-c", "--mismatch-policy", "strict", program], input);
        assert_eq!(code, 5, "strict raises for {program}");
        assert!(out.is_empty(), "strict publishes nothing for {program}");
        assert!(
            String::from_utf8_lossy(&err).contains("slice-clamped"),
            "{program}: {err:?}"
        );
    }
    // The decline: a dynamic computed bound (input-dependent) keeps its own pre-fold behavior — no clamp cell, jq's
    // ordinary backwards-range answer.
    let (code, out, err) = run(
        &["-c", "--mismatch-policy", "strict", ".[0] as $x | .[$x:($x+1)]"],
        input,
    );
    assert_eq!(code, 0, "a declined bound stays silent: {err:?}");
    assert_eq!(String::from_utf8(out).unwrap(), "[0]\n");
}

// --------------------------------------------------------------------------- The 045 W0 subcommand surface: the closed
// reserved keyword set. ---------------------------------------------------------------------------

/// `serve` is the only reserved subcommand keyword. The unused names `profile`/`test`/`bench` are ordinary programs.
#[test]
fn reserved_keywords_answer_usage_errors() {
    let (code, _, stderr) = run(&["serve"], "");
    assert_eq!(code, 2, "jqf serve without --listen must be a usage error");
    assert!(
        String::from_utf8_lossy(&stderr).contains("--listen"),
        "serve's usage error must name --listen: {}",
        String::from_utf8_lossy(&stderr)
    );
}

/// The false-suppression pins: an `alternative` in the VALUE position does not mark the PATH cells, and a `try`'s CATCH
/// handler is not a suppression region — the author's own logic fires normally.
#[test]
fn strict_does_not_over_suppress() {
    // `.["a"] = (0 // 1)` — the `//` is on the right-hand side; the path cells (index-on-null, vivification) are the
    // author's own logic and raise.
    let (code, _, err) = run(&["-c", "--mismatch-policy", "strict", ".[\"a\"] = (0 // 1)"], "null");
    assert_eq!(code, 5, "the RHS alternative does not mark the path cells");
    assert!(
        String::from_utf8_lossy(&err).contains("mismatch under strict policy"),
        "{err:?}"
    );
    // `false //.b` — the fallback's own miss is not the miss the alternative handles, so it fires.
    let (code, _, _) = run(&["-c", "--mismatch-policy", "strict", "false // .b"], "{}");
    assert_eq!(code, 5, "the fallback's own cell fires under strict");
}

/// The 052 W4 residual audit: the alternative's suppression region releases at the re-emission of a truthy output, so
/// the output's CONSUMERS run outside it — and the accepted converse corner is a multi-output left that emits a truthy
/// value before a cell-bearing one.
#[test]
fn strict_residual_shapes_fire_outside_the_alternative_region() {
    // `(0 // 1) as $v |.b` — the truthy `1` passed through `//`, the region released, and the CONSUMER's own `.b` miss
    // fires (jq answers `null`; under strict the miss is the author's unmarked site). `as $v` only holds the value.
    let (code, out, err) = run(
        &["-c", "--mismatch-policy", "strict", "(0 // 1) as $v | .b"],
        "{\"a\":1}",
    );
    assert_eq!(code, 5, "the consumer's own cell fires: {err:?}");
    assert!(out.is_empty(), "no bytes before the raise");
    assert!(String::from_utf8_lossy(&err).contains("missing-object-key"));

    // The accepted converse: a MULTI-OUTPUT left that emits a truthy value then a cell-bearing one. The `1` is
    // published (the truthy output's consumers run outside the region), then the later `.b` fires.
    let (code, out, _) = run(
        &["-c", "--mismatch-policy", "strict", "(null + 1, .b) // 5"],
        "{\"a\":1}",
    );
    assert_eq!(code, 5, "the later cell fires after the truthy prefix");
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "1\n",
        "the truthy prefix is published before the raise"
    );

    // The same program's clean converse: no cell fires, jq's own answers.
    let (code, out, _) = run(
        &["-c", "--mismatch-policy", "strict", "(null + 1, .b) // 5"],
        "{\"b\":9}",
    );
    assert_eq!(code, 0);
    assert_eq!(String::from_utf8(out).unwrap(), "1\n9\n");
}

/// The 052 W4 comparison-cell risk: `sort` over a heterogeneous array is ordinary jq, and strict must not make it
/// unusable. The immunity is structural — builtin internals compare through the comparator laws and never evaluate a
/// PROGRAM comparison node — so the whole sort family answers jq's bytes at exit 0 under strict, while an EXPLICIT `1 <
/// "a"` (the frozen cell 7) raises.
#[test]
fn strict_sort_family_is_immune_but_an_explicit_comparison_raises() {
    let input = "[1,\"a\",2]";
    for program in [
        "sort",
        "min",
        "max",
        "unique",
        "group_by(.)",
        "bsearch(\"a\")",
        "sort_by(.)",
        "unique_by(.)",
    ] {
        let (code, out, err) = run(&["-c", "--mismatch-policy", "strict", program], input);
        assert_eq!(code, 0, "builtin {program} must not fire the comparison cell: {err:?}");
        assert!(!String::from_utf8_lossy(&err).contains("mismatch"), "{program}");
        // The immunity must not cost the answer: each builtin still returns jq's bytes over [1,"a",2] (numbers order
        // below strings).
        let expected: &str = match program {
            "sort" | "unique" | "sort_by(.)" | "unique_by(.)" => "[1,2,\"a\"]\n",
            "min" | "bsearch(\"a\")" => "1\n",
            // bsearch: the first probe (index 1) is `"a"` itself, so the search hits equality immediately whichever
            // orientation runs.
            "max" => "\"a\"\n",
            "group_by(.)" => "[[1],[2],[\"a\"]]\n",
            other => unreachable!("unpinned sort-family builtin {other}"),
        };
        assert_eq!(
            String::from_utf8(out).unwrap(),
            expected,
            "{program} must answer jq's bytes under strict"
        );
    }
    // An explicit cross-kind comparison IS the frozen cell 7 and raises.
    let (code, out, err) = run(&["-c", "--mismatch-policy", "strict", "1 < \"a\""], "null");
    assert_eq!(code, 5);
    assert!(out.is_empty());
    assert!(String::from_utf8_lossy(&err).contains("cross-kind-ordering"));
}

/// The warn report aggregates a whole run into one line: ten missing keys are one `=10` count, never ten lines.
#[test]
fn warn_report_is_capped_and_aggregated() {
    let input = "{\"a\":1}\n{\"a\":1}\n{\"a\":1}\n{\"a\":1}\n{\"a\":1}\n{\"a\":1}\n{\"a\":1}\n{\"a\":1}\n{\"a\":1}\n{\"a\":1}\n";
    let (code, out, err) = run(&["-c", "--mismatch-policy", "warn", ".b"], input);
    assert_eq!(code, 0);
    let err = String::from_utf8_lossy(&err);
    let report_lines = err.lines().filter(|line| line.contains("mismatch report")).count();
    assert_eq!(report_lines, 1, "one aggregated line per run: {err}");
    assert!(err.contains("missing-object-key=10"), "{err}");
    let nulls = String::from_utf8(out).unwrap();
    assert_eq!(nulls.lines().count(), 10, "warn answers jq's value per input");
}

/// The dial is one acceptance table: the help spells the three positions and an unknown position is a usage error.
#[test]
fn mismatch_policy_is_a_one_table_dial() {
    let help = stdout(&["--help"], "");
    assert!(
        help.contains("--mismatch-policy lenient|warn|strict"),
        "help names the dial"
    );
    for value in ["lenient", "warn", "strict"] {
        let (code, _, _) = run(&["--mismatch-policy", value, "."], "null");
        assert_eq!(code, 0, "--mismatch-policy {value} is accepted");
    }
    let (code, _, err) = run(&["--mismatch-policy", "bogus", "."], "null");
    assert_eq!(code, 2, "an unknown position is a usage error");
    assert!(String::from_utf8_lossy(&err).contains("unknown --mismatch-policy value"));
}

/// A reserved keyword is reserved only in the first-positional slot with no program-looking prefix: the follow
/// precedent (`jqf --follow 'serve'` runs the program `serve`) and `-f FILE` both keep working.
#[test]
fn keywords_as_programs_via_follow_and_from_file() {
    // `--follow`'s positional IS the program: `serve` compiles as a program and fails to compile (a zero-argument call
    // to an undefined builtin), which proves the follow precedent beat the subcommand recognition.
    let (code, _, stderr) = run(&["--follow", "serve"], "");
    assert_eq!(
        code, 3,
        "jqf --follow serve must compile the PROGRAM serve, not the subcommand"
    );
    assert!(
        String::from_utf8_lossy(&stderr).contains("not defined"),
        "the follow program must be the undefined builtin serve: {}",
        String::from_utf8_lossy(&stderr)
    );
    // `-f serve` names the program FILE; the keyword never appears as a first positional and the reservation cannot
    // interfere.
    let (code, _, stderr) = run(&["-f", "serve"], "");
    assert_eq!(code, 2, "jqf -f serve must try to open the file serve");
    assert!(
        String::from_utf8_lossy(&stderr).contains("Could not open serve"),
        "-f serve must name the file it could not open: {}",
        String::from_utf8_lossy(&stderr)
    );
}

/// `--` ends OPTION parsing only; the keyword rule is a positional rule, so `jqf -- serve` is still the subcommand.
/// Options BEFORE the keyword are a usage error rather than a half-parsed run request.
#[test]
fn keyword_rules_around_double_dash_and_pre_options() {
    // item 1: `--` terminates option processing, so a keyword after it is ORDINARY jq text — `jqf -- serve` compiles
    // the program `serve` (a compile error, exactly as jq answers it: `jq -- serve` is `serve/0 is not defined`, exit
    // 3). The 045 W0 keyword slot is the plain FIRST POSITIONAL only; `--` is the documented non-keyword slot.
    let (code, _, stderr) = run(&["--", "serve"], "");
    assert_eq!(
        code,
        3,
        "jqf -- serve must compile the program `serve`, not dispatch the \
         subcommand: {}",
        String::from_utf8_lossy(&stderr)
    );
    let (code, _, stderr) = run(&["-c", "serve"], "");
    assert_eq!(code, 2, "options before a subcommand keyword must be a usage error");
    assert!(
        String::from_utf8_lossy(&stderr).contains("options cannot precede"),
        "the pre-keyword rejection must say why: {}",
        String::from_utf8_lossy(&stderr)
    );
}

/// item 4: `-h` is jq-pure — it prints help and exits 0 WITHOUT consuming the next argument, so `jqf -h.` behaves
/// exactly as `jq -h.` does (a wrapper that appends the filter after the flag keeps working). The `--help <topic>`
/// extension stays on the long form. `-h`'s page is the ONE-SCREEN summary and must stay one — the full reference lives
/// on `--help` (the generators and surface tests parse it), and the short page must point at it.
#[test]
fn short_help_flag_never_consumes_the_next_argument() {
    for args in [&["-h", "."][..], &["-h", "--", "-n"][..], &["-h", "no-such-topic"][..]] {
        let (code, out, _) = run(args, "");
        assert_eq!(code, 0, "{args:?} prints help and exits 0, like jq -h");
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("Usage: jqf"), "{args:?} prints the summary");
        assert!(
            text.lines().count() < 60,
            "{args:?} must stay one screen, got {} lines",
            text.lines().count()
        );
        assert!(
            text.contains("--help "),
            "the short page must point at the full reference"
        );
    }
    // The long-form topic surface is untouched: a real topic still pages, an unknown one is still the 053 usage error.
    let (code, out, _) = run(&["--help", "json"], "");
    assert_eq!(code, 0, "--help <topic> still pages");
    assert!(!out.is_empty());
    let (code, _, stderr) = run(&["--help", "no-such-topic"], "");
    assert_eq!(code, 2, "--help <unknown topic> is still a usage error");
    assert!(String::from_utf8_lossy(&stderr).contains("unknown help topic"));
}

/// items 1 and 3: `--` ends option processing and `-L` takes the attached form. `jqf -- -n` compiles the program `-n`
/// (jq's law: a compile error, exit 3, `n/0 is not defined`); `-Ldir` and `-nL dir` both register the library path.
#[test]
fn double_dash_ends_option_processing_and_l_attaches() {
    let (code, _, _) = run(&["--", "-n"], "");
    assert_eq!(code, 3, "-- -n compiles the program -n, exactly as jq answers it");
    // -Ldir attached and -nL dir clustered both accept the path and run.
    for args in [&["-nL", "/tmp", "1"][..], &["-n", "-L/tmp", "1"][..]] {
        let (code, out, _) = run(args, "");
        assert_eq!(code, 0, "{args:?} runs");
        assert_eq!(String::from_utf8_lossy(&out).trim(), "1");
    }
    // A cluster ending in a value-taking flag keeps the authored order: `-nf FILE` is `-n -f FILE`, never `-f` stealing
    // `-n`.
    let dir = std::env::temp_dir();
    let file = dir.join(format!("jqf-081-prog-{}.jq", std::process::id()));
    std::fs::write(&file, ".").expect("write program file");
    let (code, out, _) = run(&["-nf", file.to_str().unwrap()], "");
    let _ = std::fs::remove_file(&file);
    assert_eq!(code, 0, "-nf FILE is -n -f FILE");
    assert_eq!(String::from_utf8_lossy(&out).trim(), "null");
}

/// item 5: `-b`/`--binary` are accepted and ignored (jq's no-op on Unix, decided in the plan), and
/// `--build-configuration` is a command that prints jqf's own build facts and exits 0 without reading stdin.
#[test]
fn binary_flag_is_accepted_and_build_configuration_is_a_command() {
    for args in [&["-b"][..], &["--binary"][..]] {
        let (code, out, _) = run(args, "5");
        assert_eq!(code, 0, "{args:?} is accepted and runs");
        assert_eq!(String::from_utf8_lossy(&out).trim(), "5");
    }
    let (code, out, _) = run(&["--build-configuration"], "");
    assert_eq!(code, 0, "--build-configuration exits 0");
    let text = String::from_utf8_lossy(&out);
    for token in ["build=", "allocator=", "platform="] {
        assert!(text.contains(token), "build facts carry {token}: {text}");
    }
}

/// item 5: `--run-tests` is DELIBERATELY absent (the decision and its reason are in the plan). It is rejected as an
/// unknown option rather than half-parsed, and the help never advertises it.
#[test]
fn run_tests_is_deliberately_absent() {
    let (code, _, _) = run(&["--run-tests", "/dev/null"], "");
    assert_eq!(code, 2, "--run-tests is not a jqf surface");
    let (_, out, _) = run(&["--help"], "");
    assert!(
        !String::from_utf8_lossy(&out).contains("run-tests"),
        "the help must not advertise a flag the binary refuses"
    );
}

/// The help text documents the subcommand surface (the same table the parser reads), and `jqf serve --help` reaches the
/// same help.
#[test]
fn help_documents_the_subcommand_surface() {
    for args in [&["--help"][..], &["serve", "--help"]] {
        let (code, out, _) = run(args, "");
        assert_eq!(code, 0, "{args:?} must print help");
        let text = String::from_utf8(out).expect("help is UTF-8");
        assert!(text.contains("serve"), "{args:?} help must document the serve keyword");
        assert!(
            text.contains("Subcommands:"),
            "{args:?} help must have a Subcommands section"
        );
    }
}

/// The serve subcommand's own arg surface: exactly one program, the flags it owns, and usage errors for everything
/// else.
#[test]
fn serve_subcommand_owns_its_argument_surface() {
    // More than one program is a usage error.
    let (code, _, stderr) = run(&["serve", "--listen", "/tmp/jqf.sock", ".v", ".x"], "");
    assert_eq!(code, 2, "two programs must be a usage error");
    assert!(
        String::from_utf8_lossy(&stderr).contains("takes exactly one PROGRAM"),
        "the message must say one program: {}",
        String::from_utf8_lossy(&stderr)
    );
    // A flag the serve surface does not own is an unknown option.
    let (code, _, stderr) = run(&["serve", "--listen", "/tmp/jqf.sock", "--workers", "2", ".v"], "");
    assert_eq!(code, 2, "--workers is not a serve flag");
    assert!(
        String::from_utf8_lossy(&stderr).contains("unknown option"),
        "the message must say unknown option: {}",
        String::from_utf8_lossy(&stderr)
    );
    // A repeated --listen is a usage error.
    let (code, _, stderr) = run(
        &["serve", "--listen", "/tmp/a.sock", "--listen", "/tmp/b.sock", ".v"],
        "",
    );
    assert_eq!(code, 2, "a repeated --listen must be a usage error");
    assert!(
        String::from_utf8_lossy(&stderr).contains("--listen may only be given once"),
        "the message must say so: {}",
        String::from_utf8_lossy(&stderr)
    );
}

#[test]
fn xpath_selects_from_the_cli_over_a_real_xml_document() {
    let xml = "<catalog><item id=\"1\"><name>ada</name></item>\
               <item id=\"2\"><name>linus</name></item></catalog>";
    // The profile's elements flow into the ordinary jq pipeline (the accessor reads the located element's fact; a
    // COLLECT is the semantic construction barrier and materializes owned values, which have no document — so counting
    // through a collect is the owned-side law).
    let out = stdout(&["--input-format", "xml", "xpath(\"//name\") | .@name"], xml);
    assert_eq!(out, "\"name\"\n\"name\"\n");
    let out = stdout(&["--input-format", "xml", "[xpath(\"//name\")] | length"], xml);
    assert_eq!(out, "2\n");
    // Predicates: position and attribute equality.
    let out = stdout(&["--input-format", "xml", "xpath(\"//item[1]\") | .@attrs"], xml);
    assert_eq!(out, "{\n  \"id\": \"1\"\n}\n");
    // The 091 §2 predicate widening: comparison operators and the pure functions parse and select (`@id > 1` keeps item
    // 2; count and string-length parse).
    let out = stdout(&["--input-format", "xml", "[xpath(\"//item[@id > 1]\")] | length"], xml);
    assert_eq!(out, "1\n");
    let out = stdout(
        &["--input-format", "xml", "[xpath(\"//item[count(name) = 1]\")] | length"],
        xml,
    );
    assert_eq!(out, "2\n");
    let out = stdout(
        &[
            "--input-format",
            "xml",
            "[xpath(\"//item[string-length(@id) = 1]\")] | length",
        ],
        xml,
    );
    assert_eq!(out, "2\n");
    // A top-level FUNCTION result is a scalar (the 2026-08-09 widening): `count(//item)` answers the node-set's
    // cardinality as an exact integer, catchable like any other selector outcome. A union of a scalar with a path is an
    // XPath type error, still a compile rejection with a named message.
    let (code, out, _) = run(&["--input-format", "xml", "try xpath(\"count(//item)\") catch ."], xml);
    assert_eq!(code, 0);
    assert!(
        String::from_utf8_lossy(&out).contains('2'),
        "top-level count answers the cardinality, got {out:?}"
    );
    let (code, out, _) = run(
        &["--input-format", "xml", "try xpath(\"count(//item) | //name\") catch ."],
        xml,
    );
    assert_eq!(code, 0);
    assert!(
        String::from_utf8_lossy(&out).contains("cannot be unioned"),
        "scalar-union rejection should surface its message, got {out:?}"
    );
    // The format law: xpath over a JSON document is a named mismatch.
    let (code, out, _) = run(&["try xpath(\"//a\") catch ."], "{\"a\":1}");
    assert_eq!(code, 0);
    assert!(
        String::from_utf8_lossy(&out).contains("xpath serves xml documents"),
        "format mismatch should name both formats, got {out:?}"
    );
}

/// Facts are PROVENANCE. A read sees an attached fact only when the value IS the source node — reached by a path that
/// constructs nothing — and any operation that constructs a NEW value drops the facts, so a read over a computed value
/// is null exactly like a missing fact. Pinned once per accessor family: `.@` (node/value facts) and `.&` (markup
/// attributes).
#[test]
fn fact_reads_over_computed_values_are_null() {
    let toml = "# top note\nport = 8080 # main port\n";
    // The source node's fact reads (TOML attaches the leading block to `.@comment` and the own-line trailing comment to
    // `.@comment_inline`); the computed value's does not.
    let out = stdout(&["--input-format", "toml", "-c", ".port.@comment"], toml);
    assert_eq!(out, "[\"top note\"]\n");
    let out = stdout(&["--input-format", "toml", "-c", ".port.@comment_inline"], toml);
    assert_eq!(out, "[\"main port\"]\n");
    let out = stdout(&["--input-format", "toml", "-c", "(.port + 0) | .@comment"], toml);
    assert_eq!(out, "null\n", "an arithmetic result is a new value");
    let out = stdout(&["--input-format", "toml", "-c", "{k: .port} | .@comment"], toml);
    assert_eq!(out, "null\n", "a constructed object member is a new value");
    // The same law for the `.&` markup-attribute family: the source node answers, the computed value is null (a string
    // literal over `-n` is a constructed value with no document behind it).
    let xml = "<a href=\"https://x\">y</a>";
    let out = stdout(&["--input-format", "xml", "-c", ".&href"], xml);
    assert_eq!(out, "\"https://x\"\n");
    let out = stdout(&["-n", "--input-format", "xml", "-c", "\"y\" | .&href"], "");
    assert_eq!(out, "null\n", "a computed value carries no attributes");
}

#[test]
fn html_attribute_accessor_and_attrs_map_agree() {
    let html = "<a href=\"https://x\">y</a>";
    let href = stdout(&["--input-format", "html", "-c", "css(\"a\") | .&href"], html);
    let attrs = stdout(&["--input-format", "html", "-c", "css(\"a\") | .@attrs"], html);
    assert_eq!(href, "\"https://x\"\n");
    assert!(
        attrs.contains("\"href\":\"https://x\"") || attrs.contains("\"href\": \"https://x\""),
        ".@attrs must project the same href .&href reads, got {attrs}"
    );
}

/// 144 §2.2's route pair, D2: the scoped walk used to leak an earlier statement's inline comment into the NEXT
/// statement's leading set, so `.b.@comment` (scoped) answered `["inline-a"]` where the whole route answered `null`.
/// Both routes now agree that a statement owns its own inline comment.
#[test]
fn toml_scoped_and_whole_routes_agree_on_inline_comments() {
    let toml = "a = 1 # inline-a\nb = 2\n";
    let scoped = stdout(&["--input-format", "toml", "-c", ".b.@comment"], toml);
    assert_eq!(scoped, "null\n", "the scoped route: b's leading is empty");
    let whole = stdout(
        &["--input-format", "toml", "-c", "[.a.@comment_inline,.b.@comment]"],
        toml,
    );
    assert_eq!(
        whole, "[[\"inline-a\"],null]\n",
        "the whole route: a owns its own inline, b's leading is empty"
    );
}

/// 144 §4's TOML foot fixture, both routes (S1-T3): a comment run whose next token is a `[header]` attaches as the
/// PRECEDING table's foot — `.a.@comment_foot` — never the next table's leading, and the document trailer keeps its
/// root owner.
#[test]
fn toml_foot_run_attaches_to_the_preceding_table_on_both_routes() {
    let toml = "[a]\nx = 1\n# foot of a\n[b]\ny = 2\n# trailer\n";
    let scoped = stdout(&["--input-format", "toml", "-c", ".a.@comment_foot"], toml);
    assert_eq!(
        scoped, "[\"foot of a\"]\n",
        "the scoped route carries the closing table's foot"
    );
    let whole = stdout(
        &[
            "--input-format",
            "toml",
            "-c",
            "[.a.@comment_foot,.b.@comment,.@comment]",
        ],
        toml,
    );
    assert_eq!(
        whole, "[[\"foot of a\"],null,[\"trailer\"]]\n",
        "the whole route: a's foot, b has no leading, the trailer stays on root"
    );
}

/// 144 D1a: `.@comment_head` is a second spelling of the canonical `.@comment` selector, normalized AT LOWERING, so a
/// read is byte-identical on both commented YAML and commented TOML, and the quoted spelling `.@["comment_head"]`
/// matches identically (S3-T3 gate).
#[test]
fn comment_head_is_an_alias_of_comment() {
    let yaml = "# lead a\na: 1 # inline-a\n# lead b\nb: 2\n# trailer\n";
    let canonical = stdout(&["--input-format", "yaml", "-c", ".a.@comment"], yaml);
    let head = stdout(&["--input-format", "yaml", "-c", ".a.@comment_head"], yaml);
    assert_eq!(head, canonical, "the alias read must equal the canonical read");
    assert_eq!(head, "[\"lead a\"]\n");
    let quoted = stdout(&["--input-format", "yaml", "-c", r#".a.@["comment_head"]"#], yaml);
    assert_eq!(quoted, head, "the quoted alias spelling must match the direct one");

    let toml = "# top note\nport = 8080 # main port\n";
    let t_canonical = stdout(&["--input-format", "toml", "-c", ".port.@comment"], toml);
    let t_head = stdout(&["--input-format", "toml", "-c", ".port.@comment_head"], toml);
    assert_eq!(t_head, t_canonical, "the alias read must equal the canonical read");
    assert_eq!(t_head, "[\"top note\"]\n");
}

/// 144 §2.1's YAML fixture (S2-T1): the comment after a value on the SAME line belongs to that value —
/// `.a.@comment_inline` — and never leaks into the next node's leading list. `.a.@comment` keeps its own leading block
/// unchanged.
#[test]
fn yaml_inline_comment_attaches_to_its_own_value_both_routes() {
    let yaml = "# lead a\na: 1 # inline-a\n# lead b\nb: 2\n# trailer\n";
    let scoped = stdout(&["--input-format", "yaml", "-c", ".a.@comment_inline"], yaml);
    assert_eq!(
        scoped, "[\"inline-a\"]\n",
        "the scoped route: a owns its same-line comment"
    );
    let whole = stdout(
        &[
            "--input-format",
            "yaml",
            "-c",
            "[.a.@comment,.a.@comment_inline,.b.@comment,.@comment]",
        ],
        yaml,
    );
    assert_eq!(
        whole, "[[\"lead a\"],[\"inline-a\"],[\"lead b\"],null]\n",
        "the whole route: a's leading unchanged, the inline on a, b's leading
         no longer absorbs the previous line's inline, and the trailer moved
         off root's leading (it is the root's foot)"
    );
}

/// 144 §4's YAML foot fixture, both flavours plus the negative case (S2-T2 / D4's column rule): a comment indented
/// deeper than the NEXT node's column is a foot of the block that is closing — reachable as `.a.@comment_foot` — while
/// a flush-left comment stays the next node's leading. The document trailer keeps its root owner as the root's foot.
#[test]
fn yaml_foot_comment_attaches_to_the_closing_block_by_column() {
    let yaml = "a:\n  x: 1\n  # foot of a\nb: 2\n# trailer\n";
    let scoped = stdout(&["--input-format", "yaml", "-c", ".a.@comment_foot"], yaml);
    assert_eq!(scoped, "[\"foot of a\"]\n", "the scoped route: a's block foot");
    let whole = stdout(
        &[
            "--input-format",
            "yaml",
            "-c",
            "[.a.@comment,.x.@comment,.a.@comment_foot,.b.@comment,.@comment_foot]",
        ],
        yaml,
    );
    assert_eq!(
        whole, "[null,null,[\"foot of a\"],null,[\"trailer\"]]\n",
        "the whole route: a's foot, nothing on b, the trailer is the root's foot"
    );
}

#[test]
fn yaml_foot_comment_sequence_flavour_attaches_to_the_closing_block() {
    let yaml = "a:\n  - 1\n  # foot of a\nb: 2\n";
    let out = stdout(&["--input-format", "yaml", "-c", ".a.@comment_foot"], yaml);
    assert_eq!(out, "[\"foot of a\"]\n", "a block sequence's foot lands on its owner");
}

#[test]
fn yaml_flush_left_comment_stays_the_next_nodes_leading() {
    // The D4 negative: equal columns are NOT a foot — a flush-left comment belongs to the next node's leading list
    // exactly as before.
    let yaml = "a: 1\n# lead of b\nb: 2\n";
    let out = stdout(&["--input-format", "yaml", "-c", ".b.@comment"], yaml);
    assert_eq!(out, "[\"lead of b\"]\n");
    let foot = stdout(&["--input-format", "yaml", "-c", ".@comment_foot"], yaml);
    assert_eq!(foot, "null\n", "no foot exists");
}

/// Member steps on the XML document root (and any markup element array) navigate the children by element name. A single
/// match yields the element (facts preserved); repeated siblings select the ordered range; `[0]` composes over each
/// selected element's own array; an unmatched name stays the HARD array-with-string mismatch (catchable, never a silent
/// null).
///
/// The assertions read the BARE value model, so they pass `--no-json-facts`: markup answered as JSON renders its facts
/// by default, and the xq-style tree would show the names these steps are meant to prove they navigated past.
#[test]
fn xml_member_steps_navigate_children_by_name() {
    let bare = ["--input-format", "xml", "--no-json-facts", "-c"];
    let xml = "<a><b>1</b><c>2</c></a>";
    let out = stdout(&[bare.as_slice(), &[".b"]].concat(), xml);
    assert_eq!(out, "[\"1\"]\n");
    let out = stdout(&[bare.as_slice(), &[".b | .@name"]].concat(), xml);
    assert_eq!(out, "\"b\"\n", "the selected element keeps its name fact");
    let out = stdout(&[bare.as_slice(), &[".b[0]"]].concat(), xml);
    assert_eq!(out, "\"1\"\n", "[0] composes over the selected element's own array");
    // Repeated siblings select the ordered range; the mismatch stays hard.
    let out = stdout(&[bare.as_slice(), &[".b"]].concat(), "<a><b>1</b><b>2</b></a>");
    assert_eq!(out, "[\"1\"]\n[\"2\"]\n");
    let (code, out, _) = run(&["--input-format", "xml", "try .x catch ."], xml);
    assert_eq!(code, 0);
    assert_eq!(
        String::from_utf8_lossy(&out),
        "\"Cannot index array with string (\\\"x\\\")\"\n"
    );
    // Nested member steps navigate name-by-name over the selected element.
    let out = stdout(&[bare.as_slice(), &[".b.c"]].concat(), "<a><b><c>3</c></b></a>");
    assert_eq!(out, "[\"3\"]\n");
}

/// The css/1 registry seam is registered with its own format law.
#[test]
fn css_is_registered_with_its_own_format_law() {
    let (code, out, _) = run(&["try css(\"div.item\") catch ."], "{\"a\":1}");
    assert_eq!(code, 0);
    assert!(
        String::from_utf8_lossy(&out).contains("css serves html documents"),
        "css mismatch should name the html format, got {out:?}"
    );
}

/// 091 §4: the html.fragment@1 input dialect is selectable from the CLI (the registered fragment identity in the
/// acceptance table) and parses under the WHATWG fragment algorithm with the fixed div context: sibling elements
/// survive, the fragment value is the bare html root's children (no implied head/body wrapper), and a plain-text
/// fragment is accepted.
#[test]
fn html_fragment_dialect_is_selectable_and_parses_fragments() {
    let out = stdout(
        &[
            "--input-format",
            "html",
            "--input-dialect",
            "html.fragment@1",
            "--no-json-facts",
            "-c",
            ".",
        ],
        "<li>one</li><li>two</li>",
    );
    assert_eq!(out, "[[\"one\"],[\"two\"]]\n");
    // The context contract: plain text is a valid fragment under the div context (no document wrapper required), and
    // the root carries the html element's name fact.
    let out = stdout(
        &[
            "--input-format",
            "html",
            "--input-dialect",
            "html.fragment@1",
            "-c",
            ".[0]",
        ],
        "plain text",
    );
    assert_eq!(out, "\"plain text\"\n");
    let out = stdout(
        &[
            "--input-format",
            "html",
            "--input-dialect",
            "html.fragment@1",
            "-c",
            ".@name",
        ],
        "x",
    );
    assert_eq!(out, "\"html\"\n");
    // The pair law: html.fragment@1 without --input-format html is an invalid pair, not a silently accepted dialect.
    let (code, _, _) = run(&["--input-dialect", "html.fragment@1", "-c", "."], "null");
    assert_eq!(code, 2, "fragment without html input is a pair error");
}

/// 091 §3: `--max-rss` accepts the sibling memory dials' suffix law. `10M` and `1%` parse; `0` (suffixed or not) stays
/// "disabled"; a bare suffix is a usage error; a small suffixed ceiling RESOLVES to bytes (the governor's refusal names
/// the resolved count, exit 5 — not a parse error).
#[test]
fn max_rss_accepts_size_suffixes_and_percent() {
    // Large suffixed ceilings, a percent, and zero all parse and complete.
    for value in ["100M", "100m", "1G", "1%", "0", "0K", "0M"] {
        let (code, _, err) = run(&["--max-rss", value, "-c", "."], "null");
        assert_eq!(
            code,
            0,
            "--max-rss {value:?} must parse, stderr: {}",
            String::from_utf8_lossy(&err)
        );
    }
    // A small suffixed ceiling is a REFUSAL with the resolved byte count named — proof the suffix multiplied, not a
    // usage error.
    for (value, bytes) in [("2K", "2048"), ("2M", "2097152")] {
        let (code, _, err) = run(&["--max-rss", value, "-c", "."], "null");
        assert_eq!(code, 5, "--max-rss {value:?} must be refused at runtime");
        let err = String::from_utf8_lossy(&err);
        assert!(
            err.contains(&format!("{bytes}-byte ceiling")),
            "--max-rss {value:?} must resolve to {bytes} bytes, got {err:?}"
        );
    }
    for value in ["k", "m", "10x", "abc", "-", "10MB"] {
        let (code, _, _) = run(&["--max-rss", value, "-c", "."], "null");
        assert_eq!(code, 2, "--max-rss {value:?} must be a usage error");
    }
}

// --------------------------------------------------------------------------- The html codec end-to-end
// ---------------------------------------------------------------------------

/// 's auto-default turns the facts projection on for xml/html input with JSON output — but the projection is a NO-OP
/// for computed-value programs (their answers are computed numbers carrying no document facts), so the rewrite must not
/// forfeit their route. The explicit `--json-facts` flag still rewrites — its semantics are identical on the computed
/// number, but the flag's contract is unconditional.
#[test]
fn auto_json_facts_does_not_forfeit_the_xml_length_route() {
    let xml = "<catalog><item id=\"1\">a</item><item id=\"2\">b</item></catalog>";
    // Default flags: auto-json-facts is active (xml input, JSON output), and `length` must still answer — the
    // whole-document route serves it now that the count rung is gone.
    let (code, _, _) = run(&["--explain", "--input-format", "xml", "length"], xml);
    assert_eq!(code, 0);
    // The answer itself: two items, not merely two routes that agree.
    let answer = stdout(&["--input-format", "xml", "length"], xml);
    assert_eq!(answer, "2\n", "the count answers the catalog's item count");
    // The count answer is byte-identical with and without the explicit dial.
    let with_dial = stdout(&["--json-facts", "--input-format", "xml", "length"], xml);
    let without_dial = stdout(&["--no-json-facts", "--input-format", "xml", "length"], xml);
    assert_eq!(with_dial, without_dial, "json_facts is a no-op on the count");
}

/// `--input-format html` decodes a WHATWG-recovered document through the ordinary value model, and `css/1` selects
/// elements over it from the CLI.
#[test]
fn html_decodes_and_css_selects_from_the_cli() {
    let html = "<!DOCTYPE html><html><head><title>t</title></head>\
                <body><ul class=\"nav\"><li id=\"a\">one</li><li>two</li></ul></body></html>";
    let out = stdout(&["--input-format", "html", "--no-json-facts", "."], html);
    // The recovered shape: html = [head [title [t]], body [ul [li [one] li [two]]]]. Read bare: markup answered as JSON
    // renders its facts by default, and this assertion is about the value model the recovery produced.
    assert_eq!(
        out,
        "[
  [
    [
      \"t\"
    ]
  ],
  [
    [
      [
        \"one\"
      ],
      [
        \"two\"
      ]
    ]
  ]
]
",
        "the recovered document shape"
    );
    // The css/1 door over the html document: the class selector matches.
    let out = stdout(&["--input-format", "html", "css(\"ul.nav > li\") | .@name"], html);
    assert_eq!(out, "\"li\"\n\"li\"\n");
    // The id selector.
    let out = stdout(&["--input-format", "html", "css(\"#a\") | .@name"], html);
    assert_eq!(out, "\"li\"\n");
}

/// The html output format: the deterministic serialize profile re-encodes a decoded document as pinned HTML.
#[test]
fn html_output_serializes_deterministically() {
    let out = stdout(&["--input-format", "html", "--output-format", "html", "."], "<p>hi</p>");
    assert_eq!(
        out, "\u{feff}<html><head></head><body><p>hi</p></body></html>\n",
        "the deterministic serialize output"
    );
    // The source profile echoes the sealed source byte-exactly.
    let out = stdout(
        &[
            "--input-format",
            "html",
            "--output-format",
            "html",
            "--output-dialect",
            "html.source@1",
            ".",
        ],
        "<p>hi</p>",
    );
    assert_eq!(out, "<p>hi</p>\n", "the source echo output");
}

/// The 089 §1 value mapping: the XML and HTML deterministic profiles encode an arbitrary value (here a JSON-decoded
/// document) by lowering it into the element/attribute model, so `--output-format xml|html` serves values from any
/// input format, not only their own decoded documents.
#[test]
fn xml_and_html_output_encode_arbitrary_values() {
    // JSON -> XML: the deterministic profile lowers the value.
    let out = stdout(
        &[
            "--output-format",
            "xml",
            "--output-dialect",
            "xml.jqf-deterministic@1",
            ".",
        ],
        "{\"a\":1,\"b\":[2,3]}",
    );
    // The XML profile ends with its own LF and the CLI adds the item-framing newline on top (the facade framing law,
    // identical for the unchanged decoded-document path below).
    assert_eq!(
        out, "<root><a>1</a><b><item>2</item><item>3</item></b></root>\n\n",
        "json value lowered into the XML element model"
    );
    // JSON -> HTML: the document-serialize profile lowers the value (BOM first, per the profile's byte law).
    let out = stdout(
        &[
            "--output-format",
            "html",
            "--output-dialect",
            "html.document-serialize@1",
            ".",
        ],
        "{\"a\":1}",
    );
    assert_eq!(
        out, "\u{feff}<root><a>1</a></root>\n",
        "json value lowered into the HTML element model"
    );
    // An XML-decoded document still serializes through the existing byte law (the value mapping never replaces it).
    let out = stdout(
        &[
            "--input-format",
            "xml",
            "--output-format",
            "xml",
            "--output-dialect",
            "xml.jqf-deterministic@1",
            ".",
        ],
        "<a><b>1</b></a>",
    );
    assert_eq!(out, "<a><b>1</b></a>\n\n", "the unchanged decoded byte law");
}

/// The 089 §4 widening: jqfjson reads a multi-document stream of adjacent envelopes exactly as plain json does — the
/// native JSON dialect is not narrower than the format it profiles. A single envelope still reads as one document.
#[test]
fn jqfjson_reads_a_multi_document_stream() {
    let out = stdout(&["--input-format", "jqfjson", "-c", "."], "{\"a\":1}{\"b\":2}");
    assert_eq!(out, "{\"a\":1}\n{\"b\":2}\n", "adjacent envelopes");
    let out = stdout(
        &["--input-format", "jqfjson", "-s", "-c", "length"],
        "{\"a\":1}{\"b\":2}",
    );
    assert_eq!(out, "2\n", "slurp collects both envelopes");
    let out = stdout(&["--input-format", "jqfjson", "-c", "."], "{\"a\":1}");
    assert_eq!(out, "{\"a\":1}\n", "one envelope is one document");
}

/// `--help <topic>`: every spelling the discovery surface advertises — every format and dialect from `--list-formats`,
/// plus the seven fixed topics (`builtins`, `codes`, `facts`, `flags`, `generators`, `mismatch`, `diff`) — is a topic
/// that prints a non-empty page at exit 0. The topic list is DERIVED from the binary's own `--list-formats`, so a
/// format or dialect added to the acceptance tables joins the topic surface without a second list to keep in step (the
/// same law the in-crate test pins against the tables directly).
#[test]
fn help_topics_cover_every_advertised_spelling() {
    let (list_code, list_out, _) = run(&["--list-formats"], "");
    assert_eq!(list_code, 0);
    let list_text = String::from_utf8(list_out).expect("utf-8");
    let mut topics: Vec<String> = Vec::new();
    for line in list_text.lines() {
        let stripped = line.strip_prefix("  ").unwrap_or(line);
        if stripped.is_empty() {
            continue;
        }
        if stripped.contains("dialects:") {
            topics.extend(
                stripped
                    .split_once("dialects:")
                    .unwrap()
                    .1
                    .split_whitespace()
                    .map(str::to_owned),
            );
        } else if !line.starts_with("  ")
            && !stripped.starts_with("input formats:")
            && !stripped.starts_with("output formats:")
            && !stripped.starts_with("direction:")
        {
            topics.push(stripped.to_owned());
        }
    }
    for fixed in ["builtins", "codes", "facts", "flags", "generators", "mismatch", "diff"] {
        topics.push(fixed.to_owned());
    }
    assert!(
        topics.len() > 50,
        "the topic surface is the format/dialect tables plus the fixed seven, got {}",
        topics.len()
    );
    for topic in topics {
        let (code, out, err) = run(&["--help", &topic], "");
        assert_eq!(code, 0, "--help {topic} failed: {}", String::from_utf8_lossy(&err));
        assert!(!out.is_empty(), "--help {topic} printed an empty page");
    }
    // `-h` is the same flag: a topic after it works too.
    let (code, out, _) = run(&["-h", "flags"], "");
    assert_eq!(code, 0, "-h <topic> is the same surface");
    assert!(!out.is_empty());
}

/// An unknown topic is a usage error (exit 2) that lists the known topics and prints nothing to stdout — never a silent
/// fallback to the full help.
#[test]
fn help_unknown_topic_is_a_usage_error() {
    let (code, out, err) = run(&["--help", "no-such-topic"], "");
    assert_eq!(code, 2, "an unknown topic exits 2");
    assert!(out.is_empty(), "no stdout on an unknown topic");
    let err = String::from_utf8_lossy(&err);
    assert!(err.contains("unknown help topic"), "the error names the miss: {err}");
    assert!(err.contains("known topics"), "the error lists the topics: {err}");
    assert!(
        err.contains("builtins") && err.contains("rfc8259") && err.contains("yaml.block@1"),
        "the known list covers fixed topics and table spellings: {err}"
    );
}

/// The 052 W5 help half: the mismatch dial is a row of the flag table, so it appears in the GENERATED `flags` page by
/// construction, and the `mismatch` topic is its own page.
#[test]
fn help_flags_and_mismatch_show_the_052_dial() {
    for topic in ["flags", "mismatch"] {
        let (code, out, _) = run(&["--help", topic], "");
        assert_eq!(code, 0, "--help {topic} must exit 0");
        let page = String::from_utf8(out).expect("utf-8");
        assert!(
            page.contains("--mismatch-policy lenient|warn|strict"),
            "--help {topic} shows the dial: {page}"
        );
        assert!(page.contains("strict"), "--help {topic} documents the strict position");
    }
    // The full help still carries the same row (the flag table is one template the `flags` page and the full help both
    // render).
    assert!(
        stdout(&["--help"], "").contains("--mismatch-policy lenient|warn|strict"),
        "the full help documents the dial"
    );
}

// --- W2: the --schema value-schema gate --------------------------

fn write_temp(text: &str, suffix: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("jqf-schema-test-{}-{suffix}", std::process::id()));
    std::fs::write(&path, text).expect("temp file writes");
    path
}

/// The `--schema` gate's exit-code law: 0 all-valid, 3 on a validation failure (the value-schema class, distinct from a
/// usage error's 2 and a decode error's 5), and the failing value's ordered error objects published raw to stderr via
/// `halt_error`'s law. Valid values pass through to the program unchanged; a failed validation never reaches the
/// program.
#[test]
fn schema_flag_gates_each_input_value() {
    let schema = write_temp(
        r#"{"type":"object","required":["name"],"properties":{"name":{"type":"string"}}}"#,
        "gate-schema.json",
    );
    let path = schema.to_str().unwrap();
    // A valid value passes through (the default program is identity), exit 0.
    let (code, out, err) = run(&["--schema", path, "-c", "."], r#"{"name":"svc"}"#);
    assert_eq!(code, 0, "valid value exits 0: {err:?}");
    assert_eq!(out, b"{\"name\":\"svc\"}\n");
    // An invalid value exits 3 with the ordered error objects on stderr.
    let (code, out, err) = run(&["--schema", path, "-c", "."], r#"{"port":8080}"#);
    assert_eq!(code, 3, "invalid value exits 3: {err:?}");
    assert!(out.is_empty(), "no stdout on failure: {out:?}");
    let err = String::from_utf8(err).expect("utf-8");
    assert!(
        err.contains("missing required property"),
        "stderr carries the error objects: {err}"
    );
    // A valid value still reaches the program; an invalid one does not.
    let (code, out, _) = run(&["--schema", path, "-c", ".name"], r#"{"name":"ada","extra":1}"#);
    assert_eq!(code, 0);
    assert_eq!(out, b"\"ada\"\n");
    // Earlier valid values' outputs stand before the first failure.
    let (code, out, _) = run(
        &["--schema", path, "-c", "."],
        "{\"name\":\"a\"}\n{\"port\":1}\n{\"name\":\"c\"}\n",
    );
    assert_eq!(code, 3, "first failure decides the class");
    assert_eq!(out, b"{\"name\":\"a\"}\n", "the valid prefix stays published");
}

/// `--schema` composes with `--input-format`: the schema file is always one strict JSON value-schema document, while
/// the DATA is read under the selected format. `jqf --schema s.json --input-format toml a.toml` is one command.
#[test]
fn schema_flag_composes_with_input_format() {
    let schema = write_temp(
        r#"{"type":"object","required":["name"],"properties":{"name":{"type":"string"}}}"#,
        "gate-schema.json",
    );
    let path = schema.to_str().unwrap();
    let data = write_temp("name=\"svc\"\nport=8080\n", "compose-a.toml");
    // The positional file is the DATA; the schema file is JSON regardless.
    let data_path = data.to_str().unwrap();
    let (code, out, err) = run(&["--schema", path, "--input-format", "toml", "-c", ".", data_path], "");
    assert_eq!(code, 0, "a valid TOML document passes: {err:?}");
    assert_eq!(out, b"{\"name\":\"svc\",\"port\":8080}\n");
    let bad = write_temp("port=8080\n", "compose-bad.toml");
    let bad_path = bad.to_str().unwrap();
    let (code, out, _) = run(&["--schema", path, "--input-format", "toml", "-c", ".", bad_path], "");
    assert_eq!(code, 3, "a failing TOML document exits the schema class");
    assert!(out.is_empty());
    // A malformed TOML document is a DECODE error: exit 5, not 3.
    let garbage = write_temp("not toml {\n", "compose-garbage.toml");
    let garbage_path = garbage.to_str().unwrap();
    let (code, _, _) = run(
        &["--schema", path, "--input-format", "toml", "-c", ".", garbage_path],
        "",
    );
    assert_eq!(code, 5, "a decode error stays the runtime class");
    for p in [&data, &bad, &garbage] {
        std::fs::remove_file(p).ok();
    }
}

/// The `--schema` usage errors: an unreadable or malformed schema file is a usage error (exit 2, stdin never read), the
/// flag may only be given once, and the modes with no per-input-value stream (--stream/--edit/--diff/--in-place) are
/// refused before a byte of input.
#[test]
fn schema_flag_rejects_usage_errors() {
    let (code, out, err) = run(&["--schema", "/no/such/schema.json", "-c", "."], "{}");
    assert_eq!(code, 2, "missing schema file is a usage error: {err:?}");
    assert!(out.is_empty());
    let bad = write_temp("not json", "usage-badschema.json");
    let bad_path = bad.to_str().unwrap();
    let (code, _, err) = run(&["--schema", bad_path, "-c", "."], "{}");
    assert_eq!(code, 2, "a malformed schema file is a usage error: {err:?}");
    let schema = write_temp("true", "usage-trueschema.json");
    let path = schema.to_str().unwrap();
    let (code, _, _) = run(&["--schema", path, "--schema", path, "-c", "."], "{}");
    assert_eq!(code, 2, "--schema may only be given once");
    for mode in ["--stream", "--edit", "--in-place"] {
        let (code, _, err) = run(&["--schema", path, mode, "-c", "."], "{}");
        assert_eq!(code, 2, "--schema + {mode} is a usage error: {err:?}");
    }
}

/// S22 (`.plans/077` item 2, from `043` W9): jq's `.[[$i]]` array-index-of.
///
/// An ARRAY-typed index over an array input searches the needle SEQUENCE and answers the array of matching positions
/// (`[1,2,1,2] |.[[1,2]]` is `[0,2]`) — the same law `indices/1` implements for its array/array branch (jq defines
/// `indices($i)` as `.[$i]`). jqf used to raise the index-class mismatch for every array index; the `DynVar` step now
/// owns the search. Any non-array input with an array index keeps jq's own mismatch message.
#[test]
fn array_index_of_searches_the_needle_and_keeps_the_mismatch() {
    // The search, literal and var-bound spellings.
    let (code, out, _) = run(&["-c", ".[[1,2]]"], "[1,2,1,2]");
    assert_eq!(code, 0);
    assert_eq!(String::from_utf8(out).unwrap().trim(), "[0,2]");
    let (code, out, _) = run(&["-c", "[1] as $x | .[$x]"], "[1,2,1]");
    assert_eq!(code, 0);
    assert_eq!(String::from_utf8(out).unwrap().trim(), "[0,2]");
    // A no-match answers the empty array, never null.
    let (code, out, _) = run(&["-c", ".[[0,1]]"], "[1,2,3]");
    assert_eq!(code, 0);
    assert_eq!(String::from_utf8(out).unwrap().trim(), "[]");
    // The mismatch arms keep jq's message text.
    let (code, _, err) = run(&[".[[1]]"], "\"abc\"");
    assert_eq!(code, 5, "array index over a string is the index-class mismatch");
    assert!(
        String::from_utf8(err)
            .unwrap()
            .contains("Cannot index string with array ([1])"),
        "the mismatch message must name the operand and the index value"
    );
    let (code, _, err) = run(&[".[[1]]"], "{}");
    assert_eq!(code, 5);
    assert!(
        String::from_utf8(err)
            .unwrap()
            .contains("Cannot index object with array ([1])")
    );
}

/// S19(a) (`.plans/077` item 2, from `043` W9): the callable-depth ceiling's teeth. The 512 bound lives on
/// NON-trampolined recursion — a self-call in a `+`/collect/`and` operand — where the trampoline cannot flatten the
/// continuation; the productive shapes (walk, `recurse`, tail positions) are trampolined and never approach it. This
/// pins both halves: a depth the ceiling admits ANSWERS, and a depth past it is REFUSED with the ceiling's own message
/// — never a stack abort. RE-PINNED from 512 to the current `CALLABLE_DEPTH_LIMIT` of 1000 at the plan/086-088-engine
/// base merge (raised the ceiling 512 -> 1000; the vacuity test still pinned the old number, so 600 answered where the
/// test expected a refusal). (The assessment itself — below the measured superlinear scaling cliff of the
/// non-trampolined call machinery, jq serving 50,000 — is recorded in `.plans/077`.)
#[test]
fn callable_depth_ceiling_answers_below_and_refuses_above() {
    let program = "def f($n): if $n == 0 then 0 else f($n - 1) + 1 end; f(N)";
    let under = program.replace('N', "900");
    let (code, out, err) = run(&[under.as_str()], "0");
    assert_eq!(code, 0, "a depth the ceiling admits must answer: {err:?}");
    assert_eq!(String::from_utf8(out).unwrap().trim(), "900");
    let over = program.replace('N', "1001");
    let (code, _, err) = run(&[over.as_str()], "0");
    assert_eq!(code, 5, "a depth past the ceiling must be refused: {err:?}");
    assert!(
        String::from_utf8(err.clone())
            .unwrap()
            .contains("exceeded the depth ceiling of 1000"),
        "the refusal must name the ceiling: {err:?}"
    );
}

/// The jqft-family level-composition surface: `--with-source` echoes the retained source of the output value; a
/// computed value with no retained source is a clean typed error (exit 5), never a thinner file.
#[test]
fn jqft_with_source_echoes_and_refuses_computed_values() {
    // A jqft input echoed through jqft output: the retained source is byte-identical, comment included.
    let src = "%jqft 1\n# doc intro\n{a: <p \"x\">}\n";
    let (code, out, err) = run(
        &[
            "--input-format",
            "jqft",
            "--output-format",
            "jqft",
            "--with-source",
            ".",
        ],
        src,
    );
    assert_eq!(code, 0, "the echo must serve: {err:?}");
    assert_eq!(
        String::from_utf8(out).unwrap(),
        format!("{src}\n"),
        "the facade adds one item newline to the byte-identical echo",
    );
    // A computed value has no retained source: clean typed error.
    let (code, _out, err) = run(&["--output-format", "jqft", "--with-source", ".a + 1"], "{\"a\":1}");
    assert_eq!(code, 5, "a computed value must refuse the source level: {err:?}");
    assert!(
        String::from_utf8(err.clone())
            .unwrap()
            .contains("cannot supply the source level"),
        "the refusal must name the missing retention: {err:?}"
    );
    // The flags are jqft/jqfb output-only.
    let (code, _, err) = run(&["--output-format", "json", "--with-source", "."], "{}");
    assert_eq!(code, 2, "non-jqft output must refuse the flags: {err:?}");
}

/// The jqfb CLI surface: a jqft document converts to the binary image and back through the ordinary input route,
/// preserving the value.
#[test]
fn jqfb_cli_surface_round_trips() {
    let src = "%jqft 1\n{name: \"ada\", id: 1, items: [\"a\", \"b\"]}\n";
    let (code, image, err) = run(&["--input-format", "jqft", "--output-format", "jqfb", "."], src);
    assert_eq!(code, 0, "jqft -> jqfb: {err:?}");
    assert!(
        image.starts_with(b"jqfb"),
        "the output must be a jqfb image, got {} bytes",
        image.len()
    );
    let (code, out, err) = run_bytes(
        &[],
        &["--input-format", "jqfb", "--output-format", "json", ".id"],
        &image,
    );
    assert_eq!(code, 0, "jqfb -> json: {err:?}");
    assert_eq!(String::from_utf8(out).unwrap().trim(), "1");
}

/// 's `~inputs` design ruling: the resident input cursor is scoped to the `-n` null-first drive — a cursor over the
/// shared input sequence collides with the per-element cursor-store reset — so every other model is a usage rejection
/// (exit 2) before a byte is read. The generic `~cursor(inputs)` spelling is NOT marked: it keeps jq's own
/// inputs-under-per-input behavior, and only the named resident owns the `-n` contract. The `-n` gate itself (first two
/// values pinned, third unread) is pinned in the compat corpus's `generators-n` rows.
#[test]
fn the_inputs_cursor_is_scoped_to_the_null_first_drive() {
    let program = "~inputs as ~i | [~i.next]";
    // The gate: `-n` serves the resident over a 3-value stream.
    let (code, out, _) = run(&["-c", "-n", program], "1\n2\n3\n");
    assert_eq!(code, 0, "-n must serve ~inputs");
    assert_eq!(String::from_utf8_lossy(&out), "[1]\n");
    // Every other model rejects with the usage class and names the contract.
    for (args, input) in [
        (vec!["-c", program], "1\n2\n3\n"),
        (vec!["-c", "-s", program], "1\n2\n3\n"),
        (vec!["-c", "--input-format", "ndjson", program], "1\n2\n3\n"),
    ] {
        let (code, out, err) = run(&args, input);
        assert_eq!(code, 2, "a per-input model must reject ~inputs: {err:?}");
        assert!(out.is_empty(), "rejection must publish no bytes");
        let text = String::from_utf8_lossy(&err);
        assert!(
            text.contains("`~inputs` is served only under `-n`/`--null-input`"),
            "the message must name the contract: {text}"
        );
    }
    // The generic spelling is untouched: `~cursor(inputs)` keeps jq's own inputs-under-per-input behavior on the
    // per-input drive.
    let (code, out, _) = run(&["-c", "~cursor(inputs) as ~i | [~i.next]"], "1\n2\n3\n");
    assert_eq!(code, 0, "the generic cursor over inputs is not the resident");
    // Element 1's cursor pulls the REMAINING value (2); element 2's run finds the shared source exhausted — jq's own
    // inputs-under-per-input shape.
    assert_eq!(String::from_utf8_lossy(&out), "[2]\n[]\n");
}

/// A compile rejection points at the mistake: the message keeps its byte offsets, and a caret excerpt under it shows
/// the offending line and column, so the user reads the error instead of counting bytes.
#[test]
fn a_compile_rejection_carries_a_caret_excerpt() {
    let (code, _, stderr) = run(&[".a |"], "");
    assert_eq!(code, 3);
    assert_eq!(
        String::from_utf8_lossy(&stderr),
        "jqf: cannot parse program at bytes 4..4: expected expression\n  .a |\n      ^\n"
    );
    // The span class too (an undefined call), and the caret spans the name.
    let (code, _, stderr) = run(&[".x | bogus"], "null");
    assert_eq!(code, 3);
    let text = String::from_utf8_lossy(&stderr);
    assert!(
        text.contains("\n  .x | bogus\n       ^^^^^\n"),
        "the caret must underline the unresolved name: {text}"
    );
}

// --- The strictness dial : --strictness --------------------

/// The flag is accepted in all four spellings; an unknown value is a usage error (exit 2) naming the expected set,
/// exactly like --mismatch-policy.
#[test]
fn strictness_flag_acceptance_and_rejection() {
    let (code, _, _) = run(&["--strictness", "warn", "."], "1");
    assert_eq!(code, 0, "warn is accepted");
    let (code, _, _) = run(&["--strictness", "strict", "."], "1");
    assert_eq!(code, 0, "strict is accepted");
    let (code, _, _) = run(&["--strictness", "error", "."], "1");
    assert_eq!(code, 0, "error is accepted");
    let (code, _, _) = run(&["--strictness", "lenient", "."], "1");
    assert_eq!(code, 0, "lenient is accepted");
    let (_, help, _) = run(&["--help"], "");
    let help = String::from_utf8_lossy(&help);
    assert!(
        help.contains("Invalid UTF-8 in a string still refuses under `lenient`."),
        "--help must name the UTF-8 refusal under lenient: {help}"
    );

    let (code, _, err) = run(&["--strictness", "bogus", "."], "1");
    assert_eq!(code, 2, "an unknown strictness value is a usage error");
    let err = String::from_utf8_lossy(&err);
    assert!(
        err.contains("unknown --strictness value: \"bogus\" (expected error, warn, strict, or lenient)"),
        "{err}"
    );
    let (code, _, _) = run(&["--strictness", "warn", "--strictness", "strict", "."], "1");
    assert_eq!(code, 2, "the flag may only be given once");
}

/// The lenient position relaxes the strict-JSON decode refusals: a leading-zero number, a plus/dot spelling, and a
/// huge-exponent literal all decode to the catalogued lenient bytes instead of refusing, while `error` keeps refusing
/// them. The compat oracle runs live where the ruling demands byte identity.
#[test]
fn strictness_lenient_decodes_number_spellings() {
    // The number grammar. Strict refuses; lenient answers the catalogued bytes.
    if !compat_available() {
        eprintln!("skipping: compat oracle is not on PATH");
        return;
    }
    for input in ["01", "007", "00.5", "-00", "+1", ".5", "1.", "1.e5", "-.5"] {
        let (code, _, _) = run(&["."], input);
        assert_eq!(code, 5, "{input} is refused at the default strictness");
        let (code, out, _) = run(&["--strictness", "lenient", "."], input);
        assert_eq!(code, 0, "{input} decodes under lenient");
        let want = compat_oracle(input).expect("compat oracle is present");
        assert_eq!(
            String::from_utf8(out).unwrap().trim(),
            want,
            "{input} answers the catalogued lenient bytes"
        );
    }
    // The huge-exponent clamp. Strict refuses; lenient clamps to the catalogued bytes.
    for (input, want) in [
        ("1e999999999999999999999", "1.7976931348623157e+308"),
        ("-1e999999999999999999999", "-1.7976931348623157e+308"),
        ("1e-999999999999999999999", "0E-1147483646"),
        ("-1e-999999999999999999999", "-0E-1147483646"),
    ] {
        let (code, _, _) = run(&["."], input);
        assert_eq!(code, 5, "{input} is refused at the default strictness");
        let (code, out, _) = run(&["--strictness", "lenient", "."], input);
        assert_eq!(code, 0, "{input} decodes under lenient");
        assert_eq!(
            String::from_utf8(out).unwrap().trim(),
            want,
            "{input} clamps to the catalogued lenient bytes"
        );
    }
    // `snan` is accepted at every position.
    let (code, out, _) = run(&["."], "snan");
    assert_eq!(code, 0, "snan decodes at the default strictness");
    assert_eq!(String::from_utf8(out).unwrap().trim(), "null");
}

/// Invalid UTF-8 in a string refuses at every dial: no dial substitutes U+FFFD. The compat oracle is not consulted;
/// this is a catalogued refusal.
#[test]
fn strictness_lenient_still_refuses_invalid_utf8_in_a_string() {
    let quoted = b"\"\xff\"";
    let unread_junk = b"{\"ok\":1,\"junk\":\"\xff\"}";
    for input in [quoted.as_slice(), unread_junk.as_slice()] {
        for args in [&["."][..], &["--strictness", "lenient", "."][..]] {
            let (code, out, err) = run_bytes(&[], args, input);
            assert_eq!(code, 5, "{args:?} must refuse invalid UTF-8 (exit 5), input={input:?}");
            assert!(
                out.is_empty(),
                "{args:?} must publish nothing on invalid UTF-8, got {out:?}"
            );
            let err = String::from_utf8_lossy(&err);
            assert!(
                err.contains("json.invalid-utf8"),
                "{args:?} diagnostic must name json.invalid-utf8, got {err}"
            );
        }
    }
}

/// Whether the system compat binary is usable as an oracle. The oracle rows skip (with a printed reason) when it is
/// absent: cargo test must not fail for an optional external binary. Byte identity stays owned by the compat corpus,
/// which refuses to run without it.
fn compat_available() -> bool {
    std::process::Command::new("jq")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// Runs one input through the system compat binary and returns its trimmed stdout; the lenient arms' byte oracle.
/// Returns `None` when it is not on PATH.
fn compat_oracle(input: &str) -> Option<String> {
    use std::io::Write;
    let mut child = std::process::Command::new("jq")
        .args(["-c", "."])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.as_mut().unwrap().write_all(input.as_bytes()).unwrap();
    Some(
        String::from_utf8(child.wait_with_output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_string(),
    )
}

/// The `RawNulByte` demotion under the dial: the multi-value case exits 0 at `error` (the per-value continue,
/// jq-identical) and promotes to exit 5 at `strict`. A single NUL value keeps exit 5 at every level (last value
/// failed).
#[test]
fn strictness_dial_promotes_raw_nul_byte() {
    let input = "\"x\" \"ab\\u0000cd\" \"y\"";
    let (code, out, _) = run(&["--raw-output0", "."], input);
    assert_eq!(code, 0, "default continues past a NUL string like jq");
    assert_eq!(out, b"x\0y\0", "the surrounding values are published");
    let (code, out, _) = run(&["--raw-output0", "--strictness", "warn", "."], input);
    assert_eq!(code, 0, "warn keeps the continue-past exit");
    assert_eq!(out, b"x\0y\0");
    let (code, out, _) = run(&["--raw-output0", "--strictness", "strict", "."], input);
    assert_eq!(code, 5, "strict promotes the warning to the failure class");
    assert_eq!(out, b"x\0y\0", "the prefix stands; only the class changes");

    let (code, out, _) = run(&["--raw-output0", "--strictness", "strict", "."], "\"ab\\u0000cd\"");
    assert_eq!(code, 5, "a single NUL value is exit 5 at every level");
    assert!(out.is_empty());
}

/// halt is dial-exempt: `halt_error(1)` keeps its own exit code at every strictness level, and the dial never touches
/// it.
#[test]
fn strictness_dial_exempts_halt() {
    for strictness in ["error", "warn", "strict", "lenient"] {
        let (code, _, _) = run(&["--strictness", strictness, "-n", "halt_error(1)"], "");
        assert_eq!(code, 1, "halt_error(1) keeps exit 1 at {strictness}");
    }
}

/// A projection loss (a tagged value published as its bare payload) is a promotable warning: exit 0 at `error`, exit 5
/// at `strict`.
#[test]
fn strictness_dial_promotes_projection_losses() {
    let input = "a: !money 100\n"; // a local tag: payload projects, tag lost
    let (code, out, err) = run(&["--input-format", "yaml", "-c", "."], input);
    assert_eq!(code, 0, "a projection loss is a warning at the default");
    let err = String::from_utf8_lossy(&err);
    assert!(err.contains("tagged value published"), "{err}");
    let (code, _, _) = run(&["--input-format", "yaml", "-c", "--strictness", "strict", "."], input);
    assert_eq!(code, 5, "strict promotes a projection loss to exit 5");
    assert_eq!(String::from_utf8(out).unwrap(), "{\"a\":\"100\"}\n");
}

/// `--diagnostics` reports the ambient counting-allocator ledger beside the rss line: how many bytes it charged and
/// whether a ceiling is enforceable at all.
#[test]
fn diagnostics_reports_the_ambient_ledger() {
    let (_, _, stderr) = run(&["--diagnostics", "."], "{\"a\":1}");
    let stderr = String::from_utf8_lossy(&stderr);
    assert!(
        stderr
            .lines()
            .any(|line| line.starts_with("jqf: ledger: ") && line.contains("enforced=true")),
        "diagnostics must state whether the ceiling is enforceable; got:\n{stderr}"
    );
}

/// The whole-read refuses a slurp that would cross the ceiling INSTEAD of reading it: the read loop's fallible
/// `try_reserve` fails once the charged accumulation passes the ceiling, and that host failure carries the usage class
/// — exit 2, naming the buffer it could not grow. An abort (134) or an engine-side raise would mean the read site lost
/// its take bound.
#[test]
fn slurp_refuses_past_the_ceiling_without_aborting() {
    let big = (0..500_000)
        .map(|i| format!("{{\"i\":{i}}}\n"))
        .fold(String::new(), |mut out, item| {
            out.push_str(&item);
            out
        });
    let (code, _, stderr) = run(&["--max-memory-bytes", "4194304", "-s", "length"], &big);
    assert_eq!(code, 2, "the read-site refusal is the usage class");
    assert!(
        String::from_utf8_lossy(&stderr).contains("cannot grow the stdin buffer"),
        "the refusal names the read buffer, got: {stderr:?}"
    );
}

/// The whole-file read sizes its buffer from the file length (untrusted input): a file larger than the ceiling must be
/// refused at read, not abort. Same read-site law as the slurp sibling: fallible reserve, usage class, the buffer named
/// in prose.
#[test]
fn a_whole_input_larger_than_the_ceiling_is_refused_at_read() {
    let big = vec![b'x'; 8 << 20];
    let (code, _, stderr) = run_bytes(
        &[],
        &["--max-memory-bytes", "1048576", "--raw-input", "-s", "length"],
        &big,
    );
    assert_eq!(code, 2, "the read-site refusal is the usage class");
    assert!(
        String::from_utf8_lossy(&stderr).contains("cannot grow the stdin buffer"),
        "the refusal names the read buffer, got: {stderr:?}"
    );
}

/// `--explain` reports the lazy-document activity: how many container spans the codec deferred (`deferred=`) and how
/// many it materialized on demand (`materialized=`). The W3-T1 lazy default defers containers below the frontier on the
/// whole-document route, and a program that touches them materializes them; the explain ROUTE block must surface both
/// counts.
#[test]
fn explain_reports_deferred_and_materialized_counts() {
    let input = "{\"a\":{\"b\":1,\"c\":2},\"d\":{\"e\":3,\"f\":{\"g\":4}}}";
    // A real file, not the pipe: the request must take the whole-read path (the streaming pipe lane decodes from a
    // window and reports no deferral facts of its own).
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let dir = std::env::temp_dir().join(format!("jqf-explain-lazy-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("deferred.json");
    std::fs::write(&path, input).unwrap();
    let (code, _, stderr) = run(&["--explain", "-c", "..", path.to_str().unwrap()], "");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "the program must run clean");
    let text = String::from_utf8_lossy(&stderr);
    assert!(
        text.contains("deferred="),
        "--explain must print deferred=; got:\n{text}"
    );
    assert!(
        text.contains("materialized="),
        "--explain must print materialized=; got:\n{text}"
    );
}

/// `--explain` on a program that fails to parse still prints JSON-free explain lines: the parse is invalid, then a
/// recovered-tree outline of the node kinds the parser kept. The ordinary compile-error stderr and exit 3 follow. There
/// was no run, so no cost line and no `jqf: diag` object stream.
#[test]
fn explain_prints_recovered_tree_on_parse_failure() {
    let (code, stdout, stderr) = run(&["--explain", "if . then 1"], "");
    assert_eq!(code, 3, "a parse failure is exit 3");
    assert!(stdout.is_empty(), "a parse failure publishes no stdout");
    let text = String::from_utf8_lossy(&stderr);
    assert!(
        text.contains("jqf: explain: parse: invalid"),
        "--explain must name the invalid parse; got:\n{text}"
    );
    assert!(
        text.contains("jqf: explain: recovered:"),
        "--explain must print a recovered outline; got:\n{text}"
    );
    let recovered = text
        .lines()
        .find(|line| line.starts_with("jqf: explain: recovered:"))
        .expect("the recovered line is present");
    assert!(
        recovered.contains("Conditional") && recovered.contains("Number"),
        "the outline must keep the if and the then-branch; got:\n{recovered}"
    );
    let parse_at = text
        .find("jqf: explain: parse: invalid")
        .expect("parse: invalid is present");
    let error_at = text
        .find("cannot parse program")
        .expect("the compile-error stderr must still follow");
    assert!(
        parse_at < error_at,
        "the explain block prints before the compile error; got:\n{text}"
    );
    // A hole inside a call: the outline names the Error span and the later argument that survived recovery.
    let (code, _, stderr) = run(&["--explain", "f(. + ; .ok)"], "");
    assert_eq!(code, 3, "a recovery hole is still exit 3");
    let hole = String::from_utf8_lossy(&stderr);
    let hole_recovered = hole
        .lines()
        .find(|line| line.starts_with("jqf: explain: recovered:"))
        .expect("the recovered line is present for a call hole");
    assert!(
        hole_recovered.contains("Call") && hole_recovered.contains("Error@") && hole_recovered.contains("Postfix"),
        "the outline must name the hole and the surviving argument; got:\n{hole_recovered}"
    );
    for text in [&text, &hole] {
        for line in text.lines() {
            let trimmed = line.trim_start();
            assert!(
                !trimmed.starts_with('{'),
                "--explain is JSON-free; got object line:\n{line}"
            );
            assert!(
                !trimmed.starts_with("jqf: diag"),
                "--explain must not print the diag stream; got:\n{line}"
            );
        }
        assert!(
            !text.contains("jqf: explain: cost:"),
            "a parse failure has no run and no cost line; got:\n{text}"
        );
    }
}

/// A successful `--explain` still prints the plan and route blocks and never claims the parse was invalid.
#[test]
fn explain_valid_program_keeps_plan_and_route() {
    let (code, _, stderr) = run(&["--explain", "-n", "1"], "");
    assert_eq!(code, 0, "a valid null-input program must run");
    let text = String::from_utf8_lossy(&stderr);
    assert!(
        text.contains("jqf: explain: program:"),
        "a successful compile must print the plan block; got:\n{text}"
    );
    assert!(
        text.contains("jqf: explain: route:"),
        "a successful run must print the route block; got:\n{text}"
    );
    assert!(
        !text.contains("jqf: explain: parse: invalid"),
        "a valid program must not print parse: invalid; got:\n{text}"
    );
}

/// `-r` refuses every non-JSON-family output, not a hand-listed subset.
#[test]
fn raw_output_refuses_non_json_family_output() {
    for output in ["toml", "csv", "tsv", "cbor", "xml", "messagepack", "yaml", "render"] {
        let (code, _, stderr) = run(&["-r", "--output-format", output, "."], "\"x\"");
        assert_eq!(code, 2, "-r with {output} output must be a usage error");
        assert!(
            String::from_utf8_lossy(&stderr).contains("apply to JSON-family output only"),
            "unexpected message for {output}: {}",
            String::from_utf8_lossy(&stderr)
        );
    }
}

/// Every run option before a subcommand is the usage error — including the flags the old hand list missed.
#[test]
fn every_run_option_before_a_subcommand_is_a_usage_error() {
    for args in [
        vec!["-M", "serve"],
        vec!["-C", "serve"],
        vec!["--seq", "serve"],
        vec!["--mismatch-policy", "warn", "serve"],
        vec!["--csv-delimiter", ";", "serve"],
        vec!["--edit", "--check", "serve"],
    ] {
        let (code, _, stderr) = run(&args, "");
        assert_eq!(
            code,
            2,
            "{args:?} before serve must be a usage error, got {code}: {}",
            String::from_utf8_lossy(&stderr)
        );
        assert!(
            String::from_utf8_lossy(&stderr).contains("options cannot precede"),
            "{args:?} must name the prefix guard: {}",
            String::from_utf8_lossy(&stderr)
        );
    }
}

/// A record-issue ordinal is the same on a seekable file and a one-chunk pipe. The json-seq per-value line has its own
/// test (`json_seq_per_value_error_line_matches_the_unit`).
#[test]
fn record_issue_ordinal_is_cycle_invariant() {
    let dirty = "{\"a\":1}\n\n{\"a\":2}\n";
    let recovering = [
        "--input-format",
        "ndjson",
        "--input-dialect",
        "ndjson.recovering@1",
        ".",
    ];
    let (_, _, pipe_err) = run(&recovering, dirty);
    let dir = std::env::temp_dir().join(format!(
        "jqf-ordinal-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("t")
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("dirty.ndjson");
    std::fs::write(&path, dirty).expect("fixture writes");
    let path_str = path.to_str().expect("utf-8 path");
    let mut file_args = recovering.to_vec();
    file_args.push(path_str);
    let (_, _, file_err) = run(&file_args, "");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
    let pipe_text = String::from_utf8_lossy(&pipe_err);
    let file_text = String::from_utf8_lossy(&file_err);
    let pipe_record = pipe_text
        .lines()
        .find(|line| line.contains("record "))
        .expect("pipe must print a record issue");
    let file_record = file_text
        .lines()
        .find(|line| line.contains("record "))
        .expect("file must print a record issue");
    let pipe_n = pipe_record
        .split("record ")
        .nth(1)
        .and_then(|rest| rest.split([',', ' ', ')']).next());
    let file_n = file_record
        .split("record ")
        .nth(1)
        .and_then(|rest| rest.split([',', ' ', ')']).next());
    assert_eq!(
        pipe_n, file_n,
        "file vs one-chunk pipe must print the same record N\npipe: {pipe_text}\nfile: {file_text}"
    );
}

/// A config `parallel=false` must not block an explicit `--workers`.
#[test]
fn argv_workers_wins_over_config_no_parallel() {
    let dir = std::env::temp_dir().join(format!(
        "jqf-argv-wins-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("t")
    ));
    let _ = std::fs::create_dir_all(&dir);
    std::fs::write(dir.join(".jqf.toml"), "[defaults]\nparallel = false\n").expect("config writes");
    let mut command = Command::new(jqf_binary());
    command
        .env_remove("JQF_NO_CONFIG")
        .env("HOME", &dir)
        .env_remove("XDG_CONFIG_HOME")
        .current_dir(&dir)
        .args(["--workers", "2", "-n", "1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = command.output().expect("jqf runs");
    let _ = std::fs::remove_dir_all(&dir);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "argv --workers must win over config parallel=false: {stderr}"
    );
    assert!(
        !stderr.contains("--no-parallel"),
        "the error must not name a flag the user did not type: {stderr}"
    );
}

#[test]
fn record_errors_name_the_file_not_stdin() {
    let dir = std::env::temp_dir().join(format!(
        "jqf-record-name-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("t")
    ));
    let _ = std::fs::create_dir_all(&dir);
    let first = dir.join("m1.ndjson");
    let second = dir.join("m2.ndjson");
    std::fs::write(&first, "{\"a\":1}\n").expect("m1");
    std::fs::write(&second, "{\"a\":\"x\"}\n").expect("m2");
    let (_, _, stderr) = run(
        &[
            "--input-format",
            "ndjson",
            "--no-parallel",
            "-c",
            ".a + 1",
            first.to_str().expect("utf8"),
            second.to_str().expect("utf8"),
        ],
        "",
    );
    let _ = std::fs::remove_dir_all(&dir);
    let text = String::from_utf8_lossy(&stderr);
    assert!(
        text.contains("m2.ndjson"),
        "a named record file must appear in the error, got {text}"
    );
    assert!(
        !text.contains("<stdin>"),
        "a named file must not render as <stdin>: {text}"
    );
}

#[test]
fn json_seq_per_value_error_line_matches_the_unit() {
    // RS + value + LF per unit. The failing second unit sits on line 2; counting the unit's own terminator as a further
    // line put this on 3.
    let input = b"\x1e{\"id\":1}\n\x1e{\"id\":\"x\"}\n\x1e{\"id\":3}\n";
    let (_, _, stderr) = run_bytes(&[], &["--seq", ".id + 1"], input);
    let text = String::from_utf8_lossy(&stderr);
    assert!(
        text.contains("<stdin>:2"),
        "json-seq per-value error must name the unit's line, got {text}"
    );
    assert!(
        !text.contains("<stdin>:3"),
        "json-seq must not count the unit terminator as an extra line: {text}"
    );
}

/// A non-UTF-8 input path is an OS string: the parser must not reject it as illegal UTF-8. The path need not exist on
/// disk (APFS refuses `0xFF` in names); the open failure is a missing-file error, never a UTF-8 usage error.
#[cfg(unix)]
#[test]
fn non_utf8_input_path_is_not_a_utf8_usage_error() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let path = OsString::from_vec(vec![b'd', b'a', b't', 0xff, b'x']);
    let output = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .arg(".")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("jqf runs");
    let err = String::from_utf8_lossy(&output.stderr);
    assert_ne!(
        output.status.code(),
        Some(0),
        "a missing non-UTF-8 path must not succeed: {err}"
    );
    assert!(
        !err.contains("not valid UTF-8"),
        "a non-UTF-8 path must not be a UTF-8 usage error: {err}"
    );
    assert!(
        err.contains("Could not open"),
        "a missing non-UTF-8 path is a missing-file error: {err}"
    );
}

/// A positional program token that is not UTF-8 is a program-source error. The engine requires UTF-8 program text; that
/// is a program law, not a path law.
#[cfg(unix)]
#[test]
fn non_utf8_program_is_a_program_utf8_error() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let program = OsString::from_vec(vec![b'.', 0xff]);
    let output = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .arg(&program)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("jqf runs");
    let err = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(2),
        "a non-UTF-8 program is a usage error: {err}"
    );
    assert!(
        err.contains("program is not valid UTF-8"),
        "the refusal names the program, not a path: {err}"
    );
}

#[test]
fn container_spans_parse_failure_is_fail_loud() {
    let (code, stdout, stderr) = run_env(&[("JQF_CONTAINER_SPANS", "nope")], &["."], "1");
    assert_eq!(code, 2, "a bad JQF_CONTAINER_SPANS must exit 2");
    assert!(stdout.is_empty());
    let text = String::from_utf8_lossy(&stderr);
    assert!(
        text.contains("JQF_CONTAINER_SPANS"),
        "the refusal names the knob: {text}"
    );
}

#[test]
fn keyword_definition_names_match_the_call_site_law() {
    assert_eq!(stdout(&["-n", "def empty: 1; empty"], ""), "1\n");
    assert_eq!(stdout(&["-n", "def empty: [1]; empty"], ""), "[\n  1\n]\n");
    assert_eq!(stdout(&["-n", "def true: 1; true"], ""), "true\n");
    assert_eq!(stdout(&["-n", "def null: 1; null"], ""), "null\n");
    assert_eq!(stdout(&["-n", "def false: 1; false"], ""), "false\n");
    assert_eq!(stdout(&["-n", "def true($x): $x; true(5)"], ""), "5\n");
    assert_eq!(stdout(&["-n", "def empty: 1; def empty: 2; empty"], ""), "2\n");
    assert_eq!(stdout(&["-n", "def if::foo: 1; if::foo"], ""), "1\n");
    assert_eq!(stdout(&["-n", "def and::x: 3; and::x"], ""), "3\n");
    let (code, out, err) = run(&["-n", "def if: 1; 0"], "");
    assert_eq!(code, 3);
    assert!(out.is_empty());
    assert!(!err.is_empty());
    // `let` is contextual: `def let:` is a name (parity — `let` is an identifier there), and a bare `let` after it is
    // that definition.
    assert_eq!(stdout(&["-n", "def let: 1; let"], ""), "1\n");
    assert_eq!(stdout(&["-n", "let $x = 1 | $x"], ""), "1\n");
}

#[test]
fn query_time_fact_write_applies_in_memory_and_encodes() {
    // Without --edit a fact write compiles, overlays later reads, and encodes the document. JSON encode omits comments;
    // YAML reads see the write.
    assert_eq!(stdout(&["-c", r#".a.@comment = ["x"]"#], r#"{"a":1}"#), "{\"a\":1}\n");
    assert_eq!(
        stdout(&["-c", r#".a.@comment = ["x"] | .a.@comment"#], r#"{"a":1}"#),
        "[\"x\"]\n"
    );
    assert_eq!(
        stdout(
            &["--input-format", "yaml", "-c", r#".a.@comment = ["x"] | .a.@comment"#],
            "a: 1\n"
        ),
        "[\"x\"]\n"
    );
    assert_eq!(
        stdout(&["-c", r#"def f: .a.@comment; .a.@comment = ["x"] | f"#], r#"{"a":1}"#),
        "[\"x\"]\n",
        "a callable body must see the enclosing run's fact overlay"
    );
    assert_eq!(
        stdout(&["-c", r#"def f: .a.@comment = ["x"]; f | .a.@comment"#], r#"{"a":1}"#),
        "[\"x\"]\n",
        "a write inside a callable must overlay the parent's later read"
    );
    assert_eq!(
        stdout(
            &[
                "-c",
                r#"def f: if false then f else . end; (f | .a.@comment = ["x"]), .a.@comment"#
            ],
            r#"{"a":1}"#
        ),
        "{\"a\":1}\n[\"x\"]\n",
        "a recursive callable's Complete must not wipe a parent write after its Item"
    );
    assert_eq!(
        stdout(
            &[
                "-c",
                r#"def f($x): $x, (if false then f($x) else empty end); .a.@comment = ["x"] | f(.a.@comment)"#
            ],
            r#"{"a":1}"#
        ),
        "[\"x\"]\n",
        "a recursive callable argument must see the enclosing run's fact overlay"
    );
    let yaml = stdout(
        &[
            "--input-format",
            "yaml",
            "--output-format",
            "yaml",
            r#".a.@comment = ["x"]"#,
        ],
        "a: 1\n",
    );
    assert!(
        yaml.contains("# x"),
        "query-time YAML encode must emit the overlay comment, got {yaml:?}"
    );
    let toml = stdout(
        &[
            "--input-format",
            "toml",
            "--output-format",
            "toml",
            r#".a.@comment = ["x"]"#,
        ],
        "a = 1\n",
    );
    assert!(
        toml.contains("# x"),
        "query-time TOML encode must emit the overlay comment, got {toml:?}"
    );
}

/// lane 1 at the CLI: a dynamic selector WRITE now compiles and behaves exactly like its static twin — including over
/// `-n`, where both raise the same non-located-input machine error — and an unknown DYNAMIC role is a runtime rejection
/// naming the refusal, never a silent write.
#[test]
fn dynamic_selector_writes_match_their_static_twins_at_the_cli() {
    let (code_static, out_static, err_static) = run(&["-n", r#".@comment = ["x"]"#], "");
    let (code_dynamic, out_dynamic, err_dynamic) = run(&["-n", r#".@("comment") = ["x"]"#], "");
    assert_eq!(
        (code_static, code_dynamic),
        (5, 5),
        "both spellings share the fact-write law over a non-located input"
    );
    assert_eq!((out_static, out_dynamic), (Vec::<u8>::new(), Vec::<u8>::new()));
    assert_eq!(err_static, err_dynamic);
    let (code, out, err) = run(&["-c", r#""bogus" as $r | .a.@($r) = ["x"]"#], r#"{"a":1}"#);
    assert_eq!(code, 5, "unknown dynamic role raises at runtime");
    assert_eq!(out, Vec::<u8>::new());
    let err = String::from_utf8(err).expect("stderr is text");
    assert!(err.contains("unknown fact write role"), "{err}");
}

/// The `render.hist@1` dialect: the CLI accepts the spelling (parser and help derive from the same
/// `CliOutputDialect:ALL` row) and renders a JSON array of numbers as the plain-ASCII histogram frame, one item per
/// input with the facade's final LF on top.
#[test]
fn render_hist_dialect_is_accepted_advertised_and_renders_a_histogram() {
    let out = stdout(
        &["--output-format", "render", "--output-dialect", "render.hist@1", "."],
        "[0, 5, 10, 15, 20, 20, 20]",
    );
    // One frame per run (the render format is single-document); the facade appends the frame's final LF on top.
    assert_eq!(
        out,
        concat!(
            "[0, 2)   | 1 | ##############\n",
            "[2, 4)   | 0 |\n",
            "[4, 6)   | 1 | ##############\n",
            "[6, 8)   | 0 |\n",
            "[8, 10)  | 0 |\n",
            "[10, 12) | 1 | ##############\n",
            "[12, 14) | 0 |\n",
            "[14, 16) | 1 | ##############\n",
            "[16, 18) | 0 |\n",
            "[18, 20] | 3 | ########################################\n",
        ),
        "the golden histogram through the real binary"
    );
    // Empty array -> empty frame; the facade still appends the final LF.
    let out = stdout(
        &["--output-format", "render", "--output-dialect", "render.hist@1", "."],
        "[]",
    );
    assert_eq!(out, "\n", "an empty histogram publishes an empty frame + LF");

    // The help enumeration advertises the spelling (help derives from the same acceptance table), and a misspelling
    // exits 2 before reading stdin.
    let (_, help_out, _) = run(&["--help"], "");
    let help_text = String::from_utf8(help_out).expect("utf-8 help");
    assert!(
        help_text.contains("render.hist@1"),
        "--help must advertise render.hist@1"
    );
    let (code, _, _) = run(
        &["--output-format", "render", "--output-dialect", "render.hist@2", "."],
        "[]",
    );
    assert_eq!(code, 2, "an unaccepted dialect spelling is a usage error");
}

/// A non-UTF-8 token spelled like an option is the same usage error its UTF-8 twin gets: the dash-prefix rejection runs
/// on ENCODED BYTES, so acceptance never depends on the token's encoding. A non-UTF-8 PATH stays legal — only the
/// option SHAPE is rejected.
#[test]
#[cfg(unix)]
fn non_utf8_option_shaped_token_is_rejected_as_an_option() {
    use std::os::unix::ffi::OsStringExt as _;
    let mut command = Command::new(jqf_binary());
    command.env("JQF_NO_CONFIG", "1");
    command.arg(std::ffi::OsString::from_vec(b"--\xff".to_vec()));
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let output = command
        .spawn()
        .expect("jqf spawns")
        .wait_with_output()
        .expect("jqf runs");
    assert_eq!(
        output.status.code(),
        Some(2),
        "an option-shaped non-UTF-8 token is a usage error"
    );
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("unknown option"),
        "the rejection must name the unknown option, got {err}"
    );
}

/// The input-side format flag validates against the INPUT table: `render` is an output-only format, so `--input-format
/// render` is a usage error even though `--output-format render` accepts it.
#[test]
fn input_format_flag_rejects_output_only_formats() {
    let (code, _, err) = run(&["--input-format", "render", "."], "");
    assert_eq!(code, 2, "an output-only format cannot be an input format");
    let err = String::from_utf8_lossy(&err);
    assert!(
        err.contains("unknown --input-format value"),
        "the refusal must name the input-side table, got {err}"
    );
    // The control: the output side still accepts it.
    let (code, _, _) = run(&["--help-format", "render"], "");
    assert_eq!(code, 0, "--help-format render stays accepted");
}

/// The `--diff` same-file guard compares identity, not spelling: `f` and `./f` name one file and are refused exactly
/// like two equal spellings.
#[test]
fn diff_same_file_by_different_spellings_is_refused() {
    let dir = std::env::temp_dir().join(format!("jqf-diff-same-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    std::fs::write(dir.join("f"), "{\"v\":1}\n").expect("seed");
    for (old, new) in [("f", "./f"), ("./f", "f")] {
        let mut command = Command::new(jqf_binary());
        command.env("JQF_NO_CONFIG", "1");
        command
            .args(["--diff", old, new, "."])
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = command
            .spawn()
            .expect("jqf spawns")
            .wait_with_output()
            .expect("jqf runs");
        assert_eq!(output.status.code(), Some(2), "{old} vs {new} must refuse");
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(
            err.contains("--diff needs two different files"),
            "{old} vs {new}: the refusal must be the same-file guard, got {err}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A mid-read failure names READING, not opening: a directory opens fine on Unix and fails at the first read with
/// EISDIR, and a script grepping stderr must be able to tell the two apart.
#[test]
#[cfg(unix)]
fn directory_input_reports_a_read_failure_not_an_open_failure() {
    let dir = std::env::temp_dir().join(format!("jqf-readdir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let subdir = dir.join("d");
    std::fs::create_dir_all(&subdir).expect("subdir");
    let (_, _, err) = run(&[".", subdir.to_str().expect("utf8 path")], "");
    let err = String::from_utf8_lossy(&err);
    assert!(
        err.contains("Could not read file"),
        "a mid-read failure must say READ, got {err}"
    );
    assert!(
        !err.contains("Could not open file"),
        "the open arm's wording must not leak into the read arm: {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The serve dials are once-only together: `--read-timeout` repeats are the same usage error its siblings (--listen,
/// --max-rss, -f) raise, never a silent last-wins.
#[test]
fn serve_read_timeout_is_once_only_like_its_siblings() {
    let (code, _, err) = run(
        &[
            "serve",
            "--listen",
            "/tmp/jqf-once-only.sock",
            "--read-timeout",
            "5",
            "--read-timeout",
            "6",
            ".",
        ],
        "",
    );
    assert_eq!(code, 2, "a repeated dial is a usage error");
    let err = String::from_utf8_lossy(&err);
    assert!(err.contains("--read-timeout may only be given once"), "{err}");
}
