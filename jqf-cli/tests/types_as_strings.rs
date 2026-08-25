//! The `--types-as-strings` dial (ruling), at the product path.
//!
//! Rich types stay the default: the document stores the temporal kinds, a rebuild re-encodes them natively, and `type`
//! answers the rich name. The opt-in flag reads every extended kind as its canonical plain text instead: `type` answers
//! `"string"`, the string operations behave as strings, and a rebuilt value re-encodes as a QUOTED string.
//!
//! The laws pinned here:
//!
//! * the `type` codomain is closed over the six jq names under the flag,
//!   sampling every program-visible value source — materialized decoded values AND builtin outputs (the time family is
//!   the only family that could mint a temporal; the test runs the whole of it and asserts the codomain, so a builtin
//!   that starts returning a rich kind fails);
//! * without the flag the same probes answer the rich names — the default
//!   is untouched;
//! * the string operations (length, slice, comparison, startswith) read
//!   the canonical text under the flag;
//! * the honesty scopes: a rebuilt TOML value re-encodes quoted under the
//!   flag and natively without; the projection warning names the flag; the warning is SILENT under the flag (the cast is
//!   the request's intent).

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
    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(stdin.as_bytes())
        .expect("stdin written");
    let output = child.wait_with_output().expect("jqf runs");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// One TOML document carrying every temporal kind, so one decode samples the whole extended surface.
const ALL_TEMPORALS: &str = "\
d = 2024-01-02
t = 03:04:05
dt = 2024-01-02T03:04:05
odt = 2024-01-02T03:04:05Z
";

/// The six jq type names — the closed codomain the flag promises.
const JQ_TYPE_NAMES: [&str; 6] = [
    "\"null\"",
    "\"boolean\"",
    "\"number\"",
    "\"string\"",
    "\"array\"",
    "\"object\"",
];

#[test]
fn the_type_codomain_is_closed_under_the_flag() {
    // Materialized decoded values: every temporal kind reads as a string.
    for member in ["d", "t", "dt", "odt"] {
        let (code, out, err) = run(
            &[
                "-c",
                "--input-format",
                "toml",
                "--types-as-strings",
                &format!(".{member} | type"),
            ],
            ALL_TEMPORALS,
        );
        assert_eq!((code, out.trim(), err.as_str()), (0, "\"string\"", ""));
    }
    // Builtin outputs: the time family is the only family that COULD mint a temporal, and the whole of it must answer a
    // jq name under the flag. A builtin that starts returning a rich kind changes `type` and fails here.
    let builtin_probes = [
        "now | type",
        "0 | gmtime | strftime(\"%Y-%m-%d\") | type",
        "\"2024-01-02T03:04:05Z\" | fromdateiso8601 | type",
        "0 | todateiso8601 | type",
        "0 | todate | type",
        "0 | gmtime | type",
        "\"2024-01-02\" | strptime(\"%Y-%m-%d\") | type",
    ];
    for program in builtin_probes {
        let (code, out, _) = run(&["-n", "-c", "--types-as-strings", program], "");
        assert_eq!(code, 0, "{program} must run under the flag");
        let answer = out.trim();
        assert!(
            JQ_TYPE_NAMES.contains(&answer),
            "{program} answered type {answer:?} — outside the jq codomain"
        );
    }
}

#[test]
fn the_default_keeps_the_rich_types() {
    // Without the flag the same decoded values answer their rich names.
    for (member, expected) in [
        ("d", "\"localdate\""),
        ("t", "\"localtime\""),
        ("dt", "\"localdatetime\""),
        ("odt", "\"offsetdatetime\""),
    ] {
        let (code, out, _) = run(
            &["-c", "--input-format", "toml", &format!(".{member} | type")],
            ALL_TEMPORALS,
        );
        assert_eq!((code, out.trim()), (0, expected), "{member}");
    }
}

#[test]
fn the_string_operations_read_the_canonical_text_under_the_flag() {
    let probes = [
        (".odt | length", "20"),
        (".odt[0:4]", "\"2024\""),
        (".odt == \"2024-01-02T03:04:05Z\"", "true"),
        (".odt | startswith(\"2024\")", "true"),
        (".odt | ltrimstr(\"2024-\")", "\"01-02T03:04:05Z\""),
        (".odt | split(\"T\") | .[0]", "\"2024-01-02\""),
        ("[.odt, \"zzz\"] | sort | .[0]", "\"2024-01-02T03:04:05Z\""),
    ];
    for (program, expected) in probes {
        let (code, out, _) = run(
            &["-c", "--input-format", "toml", "--types-as-strings", program],
            ALL_TEMPORALS,
        );
        assert_eq!((code, out.trim()), (0, expected), "{program}");
    }
}

#[test]
fn a_rebuilt_value_reencodes_quoted_under_the_flag_and_natively_without() {
    let (code, out, _) = run(
        &[
            "--input-format",
            "toml",
            "--output-format",
            "toml",
            "--types-as-strings",
            "{date: .odt}",
        ],
        ALL_TEMPORALS,
    );
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "date = \"2024-01-02T03:04:05Z\"");

    let (code, out, _) = run(
        &["--input-format", "toml", "--output-format", "toml", "{date: .odt}"],
        ALL_TEMPORALS,
    );
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "date = 2024-01-02T03:04:05Z");
}

#[test]
fn the_projection_warning_names_the_flag_and_is_silent_under_it() {
    // Default: the JSON encode projects the datetime and the warning names the escape hatch.
    let (code, out, err) = run(&["-c", "--input-format", "toml", ".odt"], ALL_TEMPORALS);
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "\"2024-01-02T03:04:05Z\"");
    assert!(
        err.contains("--types-as-strings"),
        "the warning must name the flag: {err:?}"
    );
    // Under the flag: same bytes, no warning — the cast is the request's explicit intent, not a loss the run has to
    // report.
    let (code, out, err) = run(
        &["-c", "--input-format", "toml", "--types-as-strings", ".odt"],
        ALL_TEMPORALS,
    );
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "\"2024-01-02T03:04:05Z\"");
    assert!(!err.contains("warning"), "no warning under the flag: {err:?}");
}
