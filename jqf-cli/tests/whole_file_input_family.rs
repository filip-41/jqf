//! Whole-file JSON input-family programs are served on demand.
//!
//! The eager whole-buffer cursor materialized one owned Value per record before the program ran (O(records) allocator
//! commit; ~1.24 GiB on the 148 MB corpus fixture). Whole-file JSON input-family routes now parse values through the
//! streaming cursor instead, which also moves a parse refusal inside `input`/`inputs` where `try` can catch it — jq's
//! law. These tests pin the routing: the reduce still sums every value, the non-null `input` drive still pulls the
//! shared cursor, and a malformed tail inside `try inputs catch empty` is caught instead of failing the drive before
//! the program runs.

use std::fs::File;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

static TEMP_SEQ: AtomicUsize = AtomicUsize::new(0);

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

fn run(args: &[&str], stdin: Option<&str>) -> (i32, Vec<u8>, Vec<u8>) {
    let mut command = Command::new(jqf_binary());
    command.env("JQF_NO_CONFIG", "1").args(args);
    match stdin {
        Some(bytes) => {
            let path = std::env::temp_dir().join(format!(
                "jqf-whole-file-input-family-{}-{}.json",
                std::process::id(),
                TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(&path, bytes).expect("writes the test input");
            command
                .stdin(Stdio::from(File::open(&path).expect("opens the test input")))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        }
        None => {
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
        }
    }
    let output = command.output().expect("jqf runs to completion");
    (output.status.code().unwrap_or(-1), output.stdout, output.stderr)
}

#[test]
fn whole_file_reduce_inputs_sums_every_value() {
    let input = (1..=10_000u64)
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let (code, stdout, stderr) = run(&["-n", "-c", "reduce inputs as $x (0; . + $x)"], Some(&input));
    assert_eq!(code, 0, "reduce completes: {stderr:?}");
    assert_eq!(String::from_utf8(stdout).unwrap().trim(), "50005000");
}

#[test]
fn whole_file_input_pull_answers_the_second_value() {
    let (code, stdout, _stderr) = run(&["-c", "input"], Some("1\n2\n3\n"));
    // jq's shared-cursor law: the initial input is the first value, `input` pulls and publishes the second, and
    // end-of-stream raises `break`.
    assert_eq!(code, 5);
    assert_eq!(String::from_utf8(stdout).unwrap().trim(), "2");
}

#[test]
fn whole_file_malformed_tail_is_caught_inside_try() {
    // The eager cursor failed the drive before the program ran; the on-demand cursor raises at the pull site, where
    // `try inputs catch empty` catches it and the two good values still count.
    let (code, stdout, stderr) = run(&["-n", "-c", "[try inputs catch empty] | length"], Some("1\n2\nx\n"));
    assert_eq!(code, 0, "the malformed tail is caught: {stderr:?}");
    assert_eq!(String::from_utf8(stdout).unwrap().trim(), "2");
}

#[test]
fn strict_ndjson_folds_stream_without_reading_whole() {
    //: an explicit `--input-format ndjson` fold pulls its records
    // through the streaming cursor instead of the whole-input record read (~99% of the shape's peak RSS on main). Every
    // D3 shape answers identically through it; the byte source being a regular file is the point — the piped variant
    // already streamed.
    let input = "1\n2\n3\n";
    let (code, stdout, stderr) = run(
        &[
            "--input-format",
            "ndjson",
            "-n",
            "-c",
            "reduce inputs as $x (0; . + $x)",
        ],
        Some(input),
    );
    assert_eq!(code, 0, "the reduce completes: {stderr:?}");
    assert_eq!(String::from_utf8(stdout).unwrap().trim(), "6");
    let (code, stdout, stderr) = run(
        &[
            "--input-format",
            "ndjson",
            "-n",
            "-c",
            "[foreach inputs as $x (0; . + $x)]",
        ],
        Some(input),
    );
    assert_eq!(code, 0, "the foreach completes: {stderr:?}");
    assert_eq!(String::from_utf8(stdout).unwrap().trim(), "[1,3,6]");
    // An early exit stops pulling: only the demanded bytes are ever read.
    let (code, stdout, stderr) = run(
        &["--input-format", "ndjson", "-n", "-c", "[limit(2; inputs)]"],
        Some(input),
    );
    assert_eq!(code, 0, "the limit completes: {stderr:?}");
    assert_eq!(String::from_utf8(stdout).unwrap().trim(), "[1,2]");
}

#[test]
fn the_streamed_fold_prunes_undemanded_fields() {
    //: the fold body's field demand lowers to a pulled-record
    // prune hint, so each pulled record decodes only what it reads — and answers byte-identically to the whole-decode
    // floor.
    let input = "{\"v\":1,\"w\":{\"x\":[1,2,3]}}\n{\"v\":2}\n{\"w\":9}\n";
    let (code, stdout, stderr) = run(
        &[
            "--input-format",
            "ndjson",
            "-n",
            "-c",
            "reduce inputs as $r (0; . + $r.v)",
        ],
        Some(input),
    );
    assert_eq!(code, 0, "the pruned fold completes: {stderr:?}");
    assert_eq!(String::from_utf8(stdout).unwrap().trim(), "3");
    // The unbounded shape declines the hint (whole decode) and answers identically to the floor.
    let input2 = "{\"a\":1}\n{\"b\":2}\n";
    let (code, stdout, stderr) = run(
        &["--input-format", "ndjson", "-n", "-c", "[inputs] | length"],
        Some(input2),
    );
    assert_eq!(code, 0, "the unbounded shape completes: {stderr:?}");
    assert_eq!(String::from_utf8(stdout).unwrap().trim(), "2");
}

#[test]
fn streamed_ndjson_inputs_accept_adjacent_value_framing() {
    // The RECORDED NARROWING : the streamed `-n` lane reads NDJSON records through the adjacent-value grammar — the
    // same decode the follow live-window and every piped `-n inputs` surface use — so the strict-framing faults (a
    // blank line, two texts on one line) that the whole-input record route refuses are accepted here. Payload errors
    // stay pull-site raises and remain catchable.
    let (code, stdout, stderr) = run(
        &["--input-format", "ndjson", "-n", "-c", "[inputs] | length"],
        Some("{\"a\":1}\n\n{\"a\":2} {\"a\":3}\n"),
    );
    assert_eq!(code, 0, "adjacent framing is accepted: {stderr:?}");
    assert_eq!(String::from_utf8(stdout).unwrap().trim(), "3");
    let (code, stdout, _stderr) = run(
        &["--input-format", "ndjson", "-n", "-c", "[try inputs catch \"CAUGHT\"]"],
        Some("{\"a\":1}\n{x\n"),
    );
    assert_eq!(code, 0);
    assert_eq!(String::from_utf8(stdout).unwrap().trim(), "[{\"a\":1},\"CAUGHT\"]");
}
