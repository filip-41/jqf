//! Integration coverage for 068 §2.5 — an `include` with an authored `{search: …}` resolves through THAT list, exactly
//! like an `import` and exactly as does (with `-L` at the DEFAULT dir, the authored search wins). Pre-fix
//! `process_include` computed the search list and discarded it, resolving through the `-L` chain instead.
//!
//! The corpus's `include-search` kind pins the same law byte-for-byte against jq; this test pins it without jq, on
//! every host.

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// Builds `<root>/custom/m.jq` and `<root>/default/m.jq` with the given bodies and runs jqf with `-L <root>/default`
/// over the single input `{}`.
fn run_include_search(custom: &str, default: &str, program: &str) -> Output {
    use std::sync::atomic::{AtomicU32, Ordering};
    static DIR_SEQ: AtomicU32 = AtomicU32::new(0);
    let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("jqf-include-search-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(root.join("custom")).expect("custom dir creates");
    std::fs::create_dir_all(root.join("default")).expect("default dir creates");
    std::fs::write(root.join("custom/m.jq"), custom).expect("custom module writes");
    std::fs::write(root.join("default/m.jq"), default).expect("default module writes");
    let mut command = Command::new(jqf_binary());
    command.env("JQF_NO_CONFIG", "1");
    command
        .current_dir(&root)
        .args(["-c", "-L"])
        .arg(root.join("default"))
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
    let _ = std::fs::remove_dir_all(&root);
    output
}

#[test]
fn include_with_authored_search_wins_over_the_l_chain() {
    let output = run_include_search(
        "def found: \"custom\";",
        "def found: \"default\";",
        "include \"m\" {search: \"./custom\"}; found",
    );
    assert!(output.status.success(), "authored search: {output:?}");
    assert_eq!(output.stdout, b"\"custom\"\n");
    assert!(output.stderr.is_empty(), "stderr: {output:?}");
}

#[test]
fn include_without_a_search_uses_the_l_chain() {
    // The counter-half: a bare `include` falls back to the loader defaults, so the `-L` dir wins — jq's law unchanged.
    let output = run_include_search(
        "def found: \"custom\";",
        "def found: \"default\";",
        "include \"m\"; found",
    );
    assert!(output.status.success(), "default chain: {output:?}");
    assert_eq!(output.stdout, b"\"default\"\n");
    assert!(output.stderr.is_empty(), "stderr: {output:?}");
}

#[test]
fn included_filter_parameters_are_call_by_name() {
    let output = run_include_search(
        "def add(a; b): a + b;\n",
        "def add(a; b): a + b;\n",
        r#"include "m"; add(1; 2)"#,
    );
    assert!(output.status.success(), "included filter-parameter def: {output:?}");
    assert_eq!(output.stdout, b"3\n");
}

#[test]
fn included_filter_parameter_re_lowers_per_use() {
    let output = run_include_search("def f(g): [g, g];\n", "def f(g): [g, g];\n", r#"include "m"; f(1, 2)"#);
    assert!(
        output.status.success(),
        "included filter-parameter re-lowers: {output:?}"
    );
    assert_eq!(output.stdout, b"[1,2,1,2]\n");
}

#[test]
fn imported_filter_parameters_are_call_by_name() {
    let output = run_include_search(
        "def add(a; b): a + b;\n",
        "def add(a; b): a + b;\n",
        r#"import "m" as m; m::add(1; 2)"#,
    );
    assert!(output.status.success(), "imported filter-parameter def: {output:?}");
    assert_eq!(output.stdout, b"3\n");
}

#[test]
fn included_value_parameters_still_compile() {
    let output = run_include_search(
        "def add($a; $b): $a + $b;\n",
        "def add($a; $b): $a + $b;\n",
        r#"include "m"; add(1; 2)"#,
    );
    assert!(output.status.success(), "included value-parameter def: {output:?}");
    assert_eq!(output.stdout, b"3\n");
}

#[test]
fn included_filter_parameter_is_hygienic() {
    let output = run_include_search(
        "def f(g): 5 as $x | g;\n",
        "def f(g): 5 as $x | g;\n",
        r#"include "m"; 1 as $x | f($x)"#,
    );
    assert!(output.status.success(), "included filter-parameter hygiene: {output:?}");
    assert_eq!(output.stdout, b"1\n");
}
