//! The `JQF_PROBE_UNACCOUNTED` probe switch (PROBE-ONLY, not a product path) prices the ledger's share of a workload by
//! switching every memory admission/commit/release off. It must reach BOTH ledgers: the ambient counting-allocator
//! account AND the threaded request account read the same probe-adjusted limits (jqf-cli/src/main.rs applies the switch
//! above the ambient install), so a probe run charges nothing anywhere.
//!
//! This test pins the observable. A probe run COMPLETES like any other run, and the `--diagnostics` ledger line reports
//! the ambient account's peak at its creation baseline — the signal that the ledger observed no allocations. Pre-fix
//! this was not true: the probe reached only the request account, so the ambient ledger kept accounting while the
//! request ledger
//!  and once the ambient ledger became the sole accountant every probe
//! run failed with an internal accounting invariant violation (the two ledgers could never agree, and the output-permit
//! commit underflowed the never-reserved counter under probe). Every probe run exited 5 and the switch measured
//! nothing.

use std::io::Write as _;
use std::process::{Command, Output, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

fn run_jqf(probe: bool, args: &[&str], input: &[u8]) -> Output {
    let mut command = Command::new(jqf_binary());
    command.env("JQF_NO_CONFIG", "1");
    if probe {
        command.env("JQF_PROBE_UNACCOUNTED", "1");
    } else {
        command.env_remove("JQF_PROBE_UNACCOUNTED");
    }
    let mut child = command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("jqf spawns");
    // A usage-error child exits WITHOUT reading stdin, closing the pipe mid-write; BrokenPipe is the expected race
    // there, not a test failure.
    if let Err(error) = child.stdin.take().expect("stdin is piped").write_all(input) {
        assert!(
            error.kind() == std::io::ErrorKind::BrokenPipe,
            "input writes to jqf's stdin: {error}"
        );
    }
    child.wait_with_output().expect("jqf runs to completion")
}

/// The ambient peak the `--diagnostics` ledger line reports under probe: the account's creation baseline (a few hundred
/// bytes — nothing was ever charged). The accounted path for the same fixture measures two orders of magnitude above
/// the threshold below, so the probe signal is unambiguous.
fn ledger_ambient_peak(stderr: &str) -> u64 {
    let line = stderr
        .lines()
        .find(|line| line.starts_with("jqf: ledger:"))
        .unwrap_or_else(|| panic!("--diagnostics prints the ledger line: {stderr}"));
    let token = line
        .split_whitespace()
        .find_map(|token| token.strip_prefix("ambient="))
        .unwrap_or_else(|| panic!("the ledger line names ambient=: {line}"));
    token.parse().expect("ambient is a byte count")
}

/// The probe peak must sit at the account baseline, and the accounted peak for the same fixture must sit far above it:
/// a fixture too small to allocate would make the probe assertion pass vacuously.
const PROBE_PEAK_THRESHOLD: u64 = 65_536;

/// A small deterministic document plus the identity program: enough work that the ambient ledger observes real
/// allocations on the accounted path, and nothing to hide a failure on the probe path.
fn fixture() -> &'static [u8] {
    b"{\"a\":1,\"b\":[1,2,3]}\n"
}

/// The probe switch reaches the ambient ledger: a probe run completes exit 0 with no accounting-invariant violation,
/// and the ambient peak stays at its baseline while the same fixture's accounted run charges far past it.
#[test]
fn probe_unaccounted_reaches_the_ambient_ledger() {
    let probe_run = run_jqf(true, &["--diagnostics", "."], fixture());
    let probe_stderr = String::from_utf8_lossy(&probe_run.stderr);
    assert_eq!(
        probe_run.status.code(),
        Some(0),
        "a probe run is a normal run: {probe_stderr}",
    );
    assert!(
        !probe_stderr.contains("internal resource accounting invariant violation"),
        "the probe run must not trip the accounting invariant: {probe_stderr}",
    );
    let probe_peak = ledger_ambient_peak(&probe_stderr);
    assert!(
        probe_peak < PROBE_PEAK_THRESHOLD,
        "probe ambient peak is the account baseline, not real allocations: {probe_peak}"
    );

    // The contrast arm: the same fixture without the switch charges real allocations, so the small probe peak is a
    // genuine probe effect and not a fixture that never allocates.
    let accounted = run_jqf(false, &["--diagnostics", "."], fixture());
    let accounted_stderr = String::from_utf8_lossy(&accounted.stderr);
    assert_eq!(
        accounted.status.code(),
        Some(0),
        "the accounted run is a normal run: {accounted_stderr}",
    );
    let accounted_peak = ledger_ambient_peak(&accounted_stderr);
    assert!(
        accounted_peak >= PROBE_PEAK_THRESHOLD,
        "the accounted fixture charges past the probe threshold: {accounted_peak}"
    );
}
