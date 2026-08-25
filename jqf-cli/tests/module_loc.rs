//! Integration coverage for — `$__loc__` reports the CANONICAL module path even when the `-L` search dir is reached
//! through a symlink.
//!
//! jq realpath-resolves the module it opened, so a module's `$__loc__` answers the PHYSICAL path
//! (`/private/tmp/.../real/m.jq` on macOS, where `/tmp` is a symlink), never the search-chain spelling
//! (`/tmp/.../link/m.jq`). The loader canonicalizes the winning candidate ONCE and derives BOTH the label and the
//! origin directory from it, so a nested import's `$__loc__` is canonical too. The corpus's `module-link` rows
//! (`tools/jqf-cli-jq-compat.sh`) pin the same law byte-for-byte against jq; this test pins it on hosts where the
//! corpus's own `/tmp` is not a symlink, by comparing against the path `fs:canonicalize` computes at run time.

// Unix-only by construction: the law is about symlink resolution, which the Tier-2 (Windows) build has no analogue of —
// the test compiles out there so the standing `cargo check --all-targets --target x86_64-pc-windows-msvc` keeps
// passing.
#![cfg(unix)]

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// Builds `<root>/real/` holding the given module files, symlinks `<root>/link` -> `real`, and runs jqf with `-L
/// <root>/link` over the single input `{}`. Returns the output and the canonical real dir.
fn run_through_link(modules: &[(&str, &str)], program: &str) -> (Output, std::path::PathBuf) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static DIR_SEQ: AtomicU32 = AtomicU32::new(0);
    let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("jqf-module-loc-{}-{seq}", std::process::id()));
    let real = root.join("real");
    std::fs::create_dir_all(&real).expect("temp real dir creates");
    for (name, text) in modules {
        std::fs::write(real.join(name), text).expect("module source writes");
    }
    let link = root.join("link");
    std::os::unix::fs::symlink("real", &link).expect("module symlink creates");
    let mut command = Command::new(jqf_binary());
    command.env("JQF_NO_CONFIG", "1");
    command
        .args(["-c", "-L"])
        .arg(&link)
        .arg(program)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("jqf spawns");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(b"{}\n")
        .expect("input writes");
    let output = child.wait_with_output().expect("jqf runs to completion");
    let canonical_real = std::fs::canonicalize(&real).expect("real dir canonicalizes");
    let _ = std::fs::remove_dir_all(&root);
    (output, canonical_real)
}

/// The JSON-quoted file field `$__loc__` reports for a module in `real`.
fn loc_quote(real: &std::path::Path, name: &str) -> String {
    let path = real.join(name);
    let escaped = path.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[test]
fn module_loc_reports_the_canonical_path_through_a_symlink() {
    let (output, real) = run_through_link(&[("m.jq", "def f: $__loc__;")], "include \"m\"; f");
    assert!(output.status.success(), "direct module: {output:?}");
    let expected = format!("{{\"file\":{},\"line\":1}}\n", loc_quote(&real, "m.jq"));
    assert_eq!(output.stdout, expected.as_bytes());
    assert!(output.stderr.is_empty());
}

#[test]
fn nested_import_reports_the_canonical_path_too() {
    // m.jq includes n.jq and calls its def; the `$__loc__` reference site is in n.jq, which the loader resolves from
    // m.jq's CANONICAL directory — so a fix that canonicalized only the label (not the origin dir) would fail here with
    // the link spelling.
    let (output, real) = run_through_link(
        &[("m.jq", "include \"n\";\ndef f: g;"), ("n.jq", "def g: $__loc__;")],
        "include \"m\"; f",
    );
    assert!(output.status.success(), "nested module: {output:?}");
    let expected = format!("{{\"file\":{},\"line\":1}}\n", loc_quote(&real, "n.jq"));
    assert_eq!(output.stdout, expected.as_bytes());
    assert!(output.stderr.is_empty());
}
