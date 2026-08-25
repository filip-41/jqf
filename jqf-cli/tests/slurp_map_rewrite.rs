//! The streamed `-s 'map(F) | add'` / `-s 'map(F) | length'` rewrite.
//!
//! Slurping every record into an array first is O(records) of allocator commit; the CLI rewrites the exact `map(F) |
//! add` / `map(F) | length` shape to `reduce inputs as $x (INIT;. + ([($x | F)] | AGG))`, which is byte-identical and
//! processes one record at a time. These tests pin the behavior across numeric sums, string concatenation, multi-output
//! F, empty input, and the non-rewritten slurp shapes that must stay on the eager path.

use std::fs::File;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

static TEMP_SEQ: AtomicUsize = AtomicUsize::new(0);

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

fn run(input: &[u8], args: &[&str]) -> (i32, Vec<u8>, Vec<u8>) {
    let path = std::env::temp_dir().join(format!(
        "jqf-slurp-map-{}-{}.ndjson",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, input).expect("writes the test input");
    let output = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(args)
        .stdin(Stdio::from(File::open(&path).expect("opens the test input")))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("jqf runs to completion");
    (output.status.code().unwrap_or(-1), output.stdout, output.stderr)
}

#[test]
fn map_add_sums_numbers() {
    let (code, stdout, stderr) = run(b"{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n", &["-s", "-c", "map(.n) | add"]);
    assert_eq!(code, 0, "{stderr:?}");
    assert_eq!(String::from_utf8(stdout).unwrap(), "6\n");
}

#[test]
fn map_length_counts_filtered_elements() {
    let (code, stdout, stderr) = run(
        b"{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n",
        &["-s", "-c", "map(select(.n > 1)) | length"],
    );
    assert_eq!(code, 0, "{stderr:?}");
    assert_eq!(String::from_utf8(stdout).unwrap(), "2\n");
}

#[test]
fn map_add_concatenates_strings_with_null_init() {
    let (code, stdout, stderr) = run(b"{\"s\":\"a\"}\n{\"s\":\"b\"}\n", &["-s", "-c", "map(.s) | add"]);
    assert_eq!(code, 0, "{stderr:?}");
    assert_eq!(String::from_utf8(stdout).unwrap(), "\"ab\"\n");
}

#[test]
fn map_length_counts_multi_output_f() {
    let (code, stdout, stderr) = run(
        b"{\"tags\":[1,2]}\n{\"tags\":[3]}\n",
        &["-s", "-c", "map(.tags[]) | length"],
    );
    assert_eq!(code, 0, "{stderr:?}");
    assert_eq!(String::from_utf8(stdout).unwrap(), "3\n");
}

#[test]
fn empty_input_add_is_null() {
    let (code, stdout, stderr) = run(b"", &["-s", "-c", "map(.n) | add"]);
    assert_eq!(code, 0, "{stderr:?}");
    assert_eq!(String::from_utf8(stdout).unwrap(), "null\n");
}

#[test]
fn non_map_slurp_stays_on_the_eager_path() {
    // `.[0]` is not a map-aggregate shape; it must still work (eager slurp).
    let (code, stdout, stderr) = run(b"{\"n\":1}\n{\"n\":2}\n", &["-s", "-c", ".[0].n"]);
    assert_eq!(code, 0, "{stderr:?}");
    assert_eq!(String::from_utf8(stdout).unwrap(), "1\n");
}
