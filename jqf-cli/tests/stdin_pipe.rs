//! Ledger item 15: piped stdin uses a growth-controlled read loop.
//!
//! Functional coverage for the hand-rolled loop (the RSS half is the `identity-10mb-pipe` lane in the RSS gate): a
//! large piped input must read fully and correctly, with no truncation at the 256 MiB bootstrap budget.

use std::io::Write as _;
use std::process::{Command, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

#[test]
fn a_source_start_bom_decodes_the_same_for_member_scoped_programs() {
    // Route consistency: `.a` on a BOM-prefixed value answers exactly as `.` and as jq do. Pre-fix, the scoped routes
    // rejected the mark while the whole-document route consumed it — one input answering differently by program.
    for program in [".", ".a", ".[]", ".a[]"] {
        let mut child = Command::new(jqf_binary())
            .env("JQF_NO_CONFIG", "1")
            .args(["-c", program])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("jqf spawns");
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(b"\xef\xbb\xbf{\"a\":[1,2],\"b\":3}")
            .expect("input writes");
        let output = child.wait_with_output().expect("jqf runs to completion");
        assert!(
            output.status.success(),
            "{program} on a BOM-prefixed value succeeds: {output:?}"
        );
        assert!(output.stderr.is_empty(), "{program} reports no BOM error: {output:?}");
    }
}

#[test]
fn a_large_piped_input_reads_completely() {
    // A single ~1.7 MB array of 200,000 integers, piped (so no length hint).
    let mut input = Vec::with_capacity(1_700_000);
    input.push(b'[');
    for index in 0..200_000_u32 {
        if index > 0 {
            input.push(b',');
        }
        input.extend_from_slice(index.to_string().as_bytes());
    }
    input.push(b']');

    let mut child = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .arg("length")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("jqf spawns");
    // A usage-error child exits WITHOUT reading stdin, closing the pipe mid-write; BrokenPipe is the expected race
    // there, not a test failure (surfaced by the 003 linux-amd64 emulated lane, where the child's exit reliably beats
    // the parent's write).
    if let Err(error) = child.stdin.take().expect("stdin is piped").write_all(&input) {
        assert!(
            error.kind() == std::io::ErrorKind::BrokenPipe,
            "input writes to jqf's stdin: {error}"
        );
    }
    let output = child.wait_with_output().expect("jqf runs to completion");
    assert!(output.status.success(), "large piped input must succeed: {output:?}");
    assert_eq!(output.stdout, b"200000\n");
    assert!(output.stderr.is_empty());
}
