//! The `--explain`/`--diagnostics` cost line reports what the run actually cost.
//!
//! Three of its five numbers used to be constants: `input` and `spill` had no writer anywhere in the workspace, and
//! `peak` read the request account — which the counting allocator never charges — so every run on every route printed
//! the account's own 272-byte allocation. The dead `spill` field is gone; the two that can be true are pinned here, on
//! the routes a user actually reaches: a named file, a piped stdin, and the NDJSON record route.
//!
//! The numbers themselves vary with the allocator and the build, so the assertions are the LAWS: input is exactly the
//! bytes the request read, and peak is at least the input the run held resident.

use std::fmt::Write as _;
use std::io::Write as _;
use std::process::{Command, Output, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// One `peak`/`input` pair read off an `--explain` cost line.
#[derive(Debug)]
struct Cost {
    peak: u64,
    input: u64,
}

/// The account baseline every route used to print as its peak.
const DEAD_PEAK_CEILING: u64 = 1024;

fn parse_cost(output: &Output) -> Cost {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stderr
        .lines()
        .find(|line| line.starts_with("jqf: explain: cost:"))
        .unwrap_or_else(|| panic!("an explain run must print a cost line: {stderr}"));
    let field = |name: &str| -> u64 {
        line.split_whitespace()
            .find_map(|word| word.strip_prefix(name))
            .unwrap_or_else(|| panic!("the cost line must carry {name}: {line}"))
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be a byte count: {line}"))
    };
    assert!(
        !line.contains(" spill="),
        "the dead logical-spill field must not return: {line}"
    );
    Cost {
        peak: field("peak="),
        input: field("input="),
    }
}

fn run_piped(args: &[&str], input: &[u8]) -> Output {
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
        .expect("stdin is piped")
        .write_all(input)
        .expect("input writes to jqf's stdin");
    child.wait_with_output().expect("jqf runs to completion")
}

fn run_file(args: &[&str], name: &str, input: &[u8]) -> Output {
    let path = std::env::temp_dir().join(format!("jqf-cost-{}-{name}", std::process::id()));
    std::fs::write(&path, input).expect("input file");
    let output = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(args)
        .arg(&path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("jqf runs to completion");
    let _ = std::fs::remove_file(&path);
    output
}

/// A named file's bytes are the request's input, and the peak holds them.
#[test]
fn file_input_is_charged_and_held() {
    let document = format!("[{}]", (0..4000).map(|i| i.to_string()).collect::<Vec<_>>().join(","));
    let cost = parse_cost(&run_file(&["--explain", ".[0]"], "file", document.as_bytes()));
    assert_eq!(
        cost.input,
        document.len() as u64,
        "input is exactly the bytes read: {cost:?}"
    );
    assert!(
        cost.peak >= cost.input,
        "a whole-read run holds its input resident: {cost:?}"
    );
}

/// A piped stdin is read by the streaming route, which charges its own reads.
#[test]
fn piped_stdin_is_charged() {
    let small = br#"{"a":[1,2,3]}"#;
    let cost = parse_cost(&run_piped(&["--explain", ".a[]"], small));
    assert_eq!(
        cost.input,
        small.len() as u64,
        "the streaming route charges the bytes it pulled: {cost:?}"
    );
    assert!(
        cost.peak > DEAD_PEAK_CEILING,
        "the peak is the run's own residency, not the account baseline: {cost:?}"
    );
}

/// The NDJSON record route reports the record file it read.
#[test]
fn record_route_input_is_charged() {
    let records = (0..500).fold(String::new(), |mut text, index| {
        let _ = writeln!(text, "{{\"id\":{index}}}");
        text
    });
    let cost = parse_cost(&run_file(
        &["--input-format", "ndjson", "--explain", ".id"],
        "ndjson",
        records.as_bytes(),
    ));
    assert_eq!(
        cost.input,
        records.len() as u64,
        "the record route's input is the file it read: {cost:?}"
    );
    assert!(
        cost.peak >= cost.input,
        "the record file is held resident while it is framed: {cost:?}"
    );
}

/// The line MOVES with the request: a small and a large run cannot report the same cost, which is exactly what the dead
/// constants used to do.
#[test]
fn small_and_large_runs_differ() {
    let small = br"[1,2,3]";
    let large = format!("[{}]", vec!["123456789"; 80_000].join(","));
    let small_cost = parse_cost(&run_file(&["--explain", ".[0]"], "small", small));
    let large_cost = parse_cost(&run_file(&["--explain", ".[0]"], "large", large.as_bytes()));
    assert!(
        large_cost.input > small_cost.input,
        "input follows the request: {small_cost:?} vs {large_cost:?}"
    );
    assert!(
        large_cost.peak > small_cost.peak,
        "peak follows the request: {small_cost:?} vs {large_cost:?}"
    );
}
