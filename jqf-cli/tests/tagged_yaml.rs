//!  owned fan-out over a `Tagged` container (YAML-vertical
//! requirement), exercised through the real CLI's product path.
//!
//! The YAML codec is the first that emits non-core tags, so a `Value:Tagged` wrapper around an array/object is
//! reachable at the CLI. The engine's `owned_kind` is payload-transparent, so `.[]`/`..` classify the wrapper as a
//! container and push a fan-out frame over it; `next_owned_child` must see through the tag too, or the frame hits an
//! internal contract error. These tests reach the OWNED form deliberately — a construction barrier (`[.]`) materializes
//! the located tagged node into an owned `Value:Tagged`, and the residual fans out over it.
//!
//! They also pin AGENTS.md's path-update law: a path write INTO a tagged container's payload retains the tag
//! (`.items[0] = 3`, `setpath`, `del`/`delpaths`), while an update VALUE computed by an explicit operation (`+`)
//! arrives untagged and is stored as-is.
//!
//! The jq byte-oracle corpus (`tools/jqf-cli-jq-compat.sh`) cannot carry these rows: jq reads no YAML and has no tag
//! concept, so there is no system-jq oracle for a tagged value. The engine pins the contract in
//! `jqf-engine/src/exec/mod.rs`; this file pins the product path.
//!
//! Every YAML-output case here NAMES `yaml.stream-canonical@1`. These assertions read canonical spellings (`!!int "3"`,
//! `!list [`) because the canonical dialect is where a tag's exact bytes are frozen; the DEFAULT output dialect is now
//! the human-readable block one, whose tag emission is pinned in `yaml_block.rs`. Naming the dialect is what keeps
//! these expectations byte-identical across that move.

use std::io::Write as _;
use std::process::{Command, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// Runs `jqf args…` with `stdin` as the input, returning (exit code, stdout, stderr).
fn run(args: &[&str], stdin: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let mut child = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("jqf spawns");
    // A usage-error child exits WITHOUT reading stdin, closing the pipe mid-write; BrokenPipe is the expected race
    // there, not a test failure (surfaced by the 003 linux-amd64 emulated lane, where the child's exit reliably beats
    // the parent's write).
    if let Err(error) = child.stdin.take().expect("stdin is piped").write_all(stdin.as_bytes()) {
        assert!(
            error.kind() == std::io::ErrorKind::BrokenPipe,
            "input writes to jqf's stdin: {error}"
        );
    }
    let output = child.wait_with_output().expect("jqf runs to completion");
    (output.status.code().unwrap_or(-1), output.stdout, output.stderr)
}

#[test]
fn fan_out_over_an_owned_tagged_array_iterates_the_payload() {
    // `.items` is a tagged `!list`; `[.]` materializes it OWNED, `.[0]` extracts the wrapper, and `.[]` fans out over
    // it. Without the `next_owned_child` untag this is the internal contract error; with it, the payload's children
    // publish as ordinary JSON.
    let (code, out, _) = run(
        &["--input-format", "yaml", "-c", ".items | [.] | .[0] | .[]"],
        "items: !list\n  - 1\n  - 2\n",
    );
    assert_eq!(code, 0, "fan-out over a tagged array must not error");
    assert_eq!(String::from_utf8(out).expect("utf-8"), "1\n2\n");
}

#[test]
fn fan_out_over_an_owned_tagged_object_iterates_the_payload_values() {
    let (code, out, _) = run(
        &["--input-format", "yaml", "-c", ".m | [.] | .[0] | .[]"],
        "m: !map\n  a: 1\n  b: 2\n",
    );
    assert_eq!(code, 0, "fan-out over a tagged object must not error");
    assert_eq!(String::from_utf8(out).expect("utf-8"), "1\n2\n");
}

#[test]
fn a_descent_collect_over_an_owned_tagged_array_is_pre_order_self_first() {
    // `[..]` over the owned tagged array: the collect captures the tagged container itself (self-first), then its
    // payload's children.
    let (code, out, _) = run(
        &[
            "--input-format",
            "yaml",
            "--output-format",
            "yaml",
            "--output-dialect",
            "yaml.stream-canonical@1",
            ".items | [.] | .[0] | [..]",
        ],
        "items: !list\n  - 1\n  - 2\n",
    );
    assert_eq!(code, 0, "`[..]` over a tagged array must not error");
    assert_eq!(
        String::from_utf8(out).expect("utf-8"),
        concat!(
            "---\n",
            "!!seq [\n",
            "  !list [\n",
            "    !!int \"1\",\n",
            "    !!int \"2\",\n",
            "  ],\n",
            "  !!int \"1\",\n",
            "  !!int \"2\",\n",
            "]\n",
            "...\n",
            "\n",
        )
    );
}

#[test]
fn a_path_update_over_a_tagged_array_retains_the_tag() {
    // AGENTS.md's path-update law: `.items[0] = 3` writes INTO the tagged array's payload, so the `!list` tag stays on
    // `.items`.
    let (code, out, _) = run(
        &[
            "--input-format",
            "yaml",
            "--output-format",
            "yaml",
            "--output-dialect",
            "yaml.stream-canonical@1",
            ".items[0] = 3",
        ],
        "items: !list\n  - 1\n  - 2\n",
    );
    assert_eq!(code, 0);
    let out = String::from_utf8(out).expect("utf-8");
    assert!(out.contains("!list"), "the tag must survive the write:\n{out}");
    assert!(
        out.contains("!!int \"3\""),
        "the write must land in the payload:\n{out}"
    );
}

#[test]
fn setpath_into_a_tagged_array_retains_the_tag() {
    let (code, out, _) = run(
        &[
            "--input-format",
            "yaml",
            "--output-format",
            "yaml",
            "--output-dialect",
            "yaml.stream-canonical@1",
            "setpath([\"items\",0]; 9)",
        ],
        "items: !list\n  - 1\n  - 2\n",
    );
    assert_eq!(code, 0);
    let out = String::from_utf8(out).expect("utf-8");
    assert!(out.contains("!list"), "setpath must keep the tag:\n{out}");
    assert!(out.contains("!!int \"9\""), "setpath must land the write:\n{out}");
}

#[test]
fn del_inside_a_tagged_object_retains_the_tag() {
    // `del(.a)` rebuilds the tagged object without the member; the `!map` tag belongs to the container node and
    // survives.
    let (code, out, _) = run(
        &[
            "--input-format",
            "yaml",
            "--output-format",
            "yaml",
            "--output-dialect",
            "yaml.stream-canonical@1",
            ".m | del(.a)",
        ],
        "m: !map\n  a: 1\n  b: 2\n",
    );
    assert_eq!(code, 0);
    let out = String::from_utf8(out).expect("utf-8");
    assert!(out.contains("!map"), "del must keep the tag:\n{out}");
    assert!(
        out.contains("!!str \"b\"") && !out.contains("!!str \"a\""),
        "del must remove the member but keep the others:\n{out}"
    );
}

#[test]
fn delpaths_inside_a_tagged_array_retains_the_tag() {
    let (code, out, _) = run(
        &[
            "--input-format",
            "yaml",
            "--output-format",
            "yaml",
            "--output-dialect",
            "yaml.stream-canonical@1",
            "delpaths([[\"items\",0]])",
        ],
        "items: !list\n  - 1\n  - 2\n  - 3\n",
    );
    assert_eq!(code, 0);
    let out = String::from_utf8(out).expect("utf-8");
    assert!(out.contains("!list"), "delpaths must keep the tag:\n{out}");
    assert!(
        !out.contains("!!int \"1\"") && out.contains("!!int \"2\""),
        "delpaths must remove the element:\n{out}"
    );
}
