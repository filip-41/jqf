//! The `--json-facts` projection surface, pinned at the product path.
//!
//! `--json-facts` wraps the program so every published value is rebuilt with its attached facts visible in the JSON
//! shape: XML elements become xq-style trees (element name as key, attributes as `@attr`, text as `#text`, repeated
//! elements as arrays), and other facts use the accessor spellings (`@comment`, `@tag`, `@attrs`,...). The projection
//! is lossy by design: it is a presentation of facts, not a round-trippable encoding.

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
    if let Err(error) = child.stdin.take().expect("stdin is piped").write_all(stdin.as_bytes()) {
        assert!(
            error.kind() == std::io::ErrorKind::BrokenPipe,
            "input writes to jqf's stdin: {error}"
        );
    }
    let output = child.wait_with_output().expect("jqf runs to completion");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stdout).expect("stdout is UTF-8"),
        String::from_utf8(output.stderr).expect("stderr is UTF-8"),
    )
}

#[test]
fn yaml_tagged_scalar_projects_the_tag_into_json() {
    let (code, out, err) = run(
        &["--input-format", "yaml", "--json-facts", "-c", "."],
        "price: !money 5\n",
    );
    assert_eq!(code, 0);
    assert_eq!(out, "{\"price\":{\"@tag\":\"!money\",\"value\":\"5\"}}\n");
    assert_eq!(err, "");
}

#[test]
fn yaml_tagged_object_merges_the_tag_into_the_object() {
    let (code, out, err) = run(
        &["--input-format", "yaml", "--json-facts", "-c", "."],
        "x: !money {a: 1}\n",
    );
    assert_eq!(code, 0);
    assert_eq!(out, "{\"x\":{\"@tag\":\"!money\",\"a\":1}}\n");
    assert_eq!(err, "");
}

#[test]
fn toml_comment_projects_to_the_comment_key() {
    let (code, out, err) = run(
        &["--input-format", "toml", "--json-facts", "-c", "."],
        "name = \"app\"\n# the port\nport = 8080\n",
    );
    assert_eq!(code, 0);
    assert_eq!(
        out,
        "{\"name\":\"app\",\"port\":{\"@comment\":[\"the port\"],\"value\":8080}}\n"
    );
    assert_eq!(err, "");
    // 144 D1a: the projection renders the FACTS, and there is one fact — the `comment_head` alias is a selector
    // spelling normalized at lowering, so the projected object carries exactly one "@comment" key and no
    // "@comment_head" (S3-T3 gate).
    assert_eq!(
        out.matches("\"@comment\"").count(),
        1,
        "the projection must carry exactly one @comment key: {out}"
    );
    assert!(
        !out.contains("@comment_head"),
        "the projection must never emit the alias spelling: {out}"
    );
}

/// 144 D7: the projection carries the two new position selectors — `@comment_inline` and `@comment_foot` — beside
/// `@comment`, each absent when the node carries nothing at that position.
#[test]
fn toml_comment_positions_project_to_their_keys() {
    let (code, out, err) = run(
        &["--input-format", "toml", "--json-facts", "-c", "."],
        "# the port\nport = 8080 # main port\n[server]\n  # the foot\n[database]\nname = \"db\"\n",
    );
    assert_eq!(code, 0);
    // The scalar carries leading + inline; the section carries the foot; the plain value in the second section carries
    // nothing and stays a bare scalar (absent-when-empty is the projection's contract).
    assert_eq!(
        out,
        "{\"port\":{\"@comment\":[\"the port\"],\"@comment_inline\":[\"main port\"],\"value\":8080},\"server\":{\"@comment_foot\":[\"the foot\"]},\"database\":{\"name\":\"db\"}}\n"
    );
    assert_eq!(err, "");
}

#[test]
fn xml_element_projects_an_xq_style_tree() {
    let (code, out, err) = run(
        &["--input-format", "xml", "--json-facts", "-c", "."],
        "<root><a href=\"https://x\">y</a></root>",
    );
    assert_eq!(code, 0);
    assert_eq!(out, "{\"root\":{\"a\":{\"@href\":\"https://x\",\"#text\":\"y\"}}}\n");
    assert_eq!(err, "");
}

///  pinned as intended: the two markup dials do NOT share
/// root-level paths — the SAME program answers differently under `--json-facts` (xq-style fact tree) and
/// `--no-json-facts` (bare value). The corrected help text says exactly this and points at the runtime hint instead of
/// claiming path equivalence.
#[test]
fn xml_root_answers_differ_between_the_facts_dials() {
    let input = "<r><v>1</v></r>";
    let (_, facts_out, _) = run(&["--input-format", "xml", "--json-facts", "-c", "."], input);
    let (_, bare_out, _) = run(&["--input-format", "xml", "--no-json-facts", "-c", "."], input);
    assert_ne!(
        facts_out, bare_out,
        "the dials must answer the root differently: {facts_out:?} vs {bare_out:?}"
    );
    // The runtime hint names the model when a path misses.
    let (_, _, err) = run(&["--input-format", "xml", "--json-facts", "-c", ".nope"], input);
    assert!(!err.is_empty(), "a missed path must print the runtime hint");
}

#[test]
fn xml_leaf_without_attributes_is_a_plain_value() {
    let (code, out, err) = run(
        &["--input-format", "xml", "--json-facts", "-c", "."],
        "<root><a>y</a></root>",
    );
    assert_eq!(code, 0);
    assert_eq!(out, "{\"root\":{\"a\":\"y\"}}\n");
    assert_eq!(err, "");
}

#[test]
fn xml_empty_element_is_null() {
    let (code, out, err) = run(
        &["--input-format", "xml", "--json-facts", "-c", "."],
        "<root><a/></root>",
    );
    assert_eq!(code, 0);
    assert_eq!(out, "{\"root\":{\"a\":null}}\n");
    assert_eq!(err, "");
}

#[test]
fn xml_repeated_elements_become_an_array() {
    let (code, out, err) = run(
        &["--input-format", "xml", "--json-facts", "-c", "."],
        "<root><a>1</a><a>2</a></root>",
    );
    assert_eq!(code, 0);
    assert_eq!(out, "{\"root\":{\"a\":[\"1\",\"2\"]}}\n");
    assert_eq!(err, "");
}

#[test]
fn xml_repeated_elements_with_attributes_become_object_arrays() {
    let (code, out, err) = run(
        &["--input-format", "xml", "--json-facts", "-c", "."],
        "<root><a href=\"x\">1</a><a href=\"y\">2</a></root>",
    );
    assert_eq!(code, 0);
    assert_eq!(
        out,
        "{\"root\":{\"a\":[{\"@href\":\"x\",\"#text\":\"1\"},{\"@href\":\"y\",\"#text\":\"2\"}]}}\n"
    );
    assert_eq!(err, "");
}

#[test]
fn data_keys_win_over_fact_keys() {
    let (code, out, err) = run(
        &["--input-format", "toml", "--json-facts", "-c", "."],
        "\"@comment\" = 1\n# real comment\nport = 2\n",
    );
    assert_eq!(code, 0);
    assert_eq!(
        out,
        "{\"@comment\":1,\"port\":{\"@comment\":[\"real comment\"],\"value\":2}}\n"
    );
    assert_eq!(err, "");
}

#[test]
fn plain_json_is_unchanged() {
    let (code, out, err) = run(&["--json-facts", "-c", "."], "{\"a\":1}\n");
    assert_eq!(code, 0);
    assert_eq!(out, "{\"a\":1}\n");
    assert_eq!(err, "");
}

#[test]
fn composed_program_projects_its_located_output() {
    let (code, out, err) = run(
        &["--input-format", "toml", "--json-facts", "-c", ".port"],
        "name = \"app\"\n# the port\nport = 8080\n",
    );
    assert_eq!(code, 0);
    assert_eq!(out, "{\"@comment\":[\"the port\"],\"value\":8080}\n");
    assert_eq!(err, "");
}

#[test]
fn computed_values_pass_through_unchanged() {
    let (code, out, err) = run(&["--json-facts", "-c", ". + [3]"], "[1,2]\n");
    assert_eq!(code, 0);
    assert_eq!(out, "[1,2,3]\n");
    assert_eq!(err, "");
}

#[test]
fn json_facts_is_listed_among_builtins() {
    let (code, out, err) = run(&["--list-builtins"], "");
    assert_eq!(code, 0);
    assert!(out.lines().any(|line| line == "json_facts/0"), "{out}");
    assert_eq!(err, "");
}

#[test]
fn json_facts_rejects_non_json_output_formats() {
    let (code, out, err) = run(&["--json-facts", "--output-format", "yaml", "."], "{\"a\":1}\n");
    assert_eq!(code, 2);
    assert_eq!(out, "");
    assert!(err.contains("--json-facts"), "{err}");
}

#[test]
fn json_facts_rejects_edit_lanes() {
    let (code, out, err) = run(&["--json-facts", "--edit", ".port = 9090"], "port = 8080\n");
    assert_eq!(code, 2);
    assert_eq!(out, "");
    assert!(err.contains("--json-facts"), "{err}");
}

#[test]
fn xml_renders_its_facts_by_default() {
    let (code, out, err) = run(&["--input-format", "xml", "-c", "."], "<r a=\"1\"><c>x</c><c>y</c></r>");
    assert_eq!(code, 0);
    assert_eq!(out, "{\"r\":{\"@a\":\"1\",\"c\":[\"x\",\"y\"]}}\n");
    assert_eq!(err, "");
}

#[test]
fn html_renders_its_facts_by_default() {
    let (code, out, err) = run(&["--input-format", "html", "-c", "."], "<p class=\"a\">hi</p>");
    assert_eq!(code, 0);
    assert_eq!(
        out,
        "{\"html\":{\"head\":null,\"body\":{\"p\":{\"@class\":\"a\",\"#text\":\"hi\"}}}}\n"
    );
    assert_eq!(err, "");
}

#[test]
fn no_json_facts_asks_for_the_bare_positional_value() {
    let (code, out, err) = run(
        &["--input-format", "xml", "--no-json-facts", "-c", "."],
        "<r a=\"1\"><c>x</c><c>y</c></r>",
    );
    assert_eq!(code, 0);
    assert_eq!(out, "[[\"x\"],[\"y\"]]\n");
    assert_eq!(err, "");
}

#[test]
fn the_markup_default_stays_off_where_the_projection_cannot_go() {
    let (code, out, err) = run(
        &["--input-format", "xml", "--output-format", "yaml", "."],
        "<r><c>x</c></r>",
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "- - x\n");
    let (code, out, err) = run(&["--input-format", "xml", "--stream", "-c", "."], "<r><c>x</c></r>");
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "[[0,0],\"x\"]\n[[0,0]]\n[[0]]\n");
}

#[test]
fn the_markup_default_leaves_the_other_formats_alone() {
    let (code, out, err) = run(&["--input-format", "toml", "-c", "."], "# the port\nport = 8080\n");
    assert_eq!(code, 0);
    assert_eq!(out, "{\"port\":8080}\n");
    assert_eq!(err, "");
}

#[test]
fn the_two_facts_dials_are_exclusive() {
    let (code, out, err) = run(&["--json-facts", "--no-json-facts", "."], "{\"a\":1}\n");
    assert_eq!(code, 2);
    assert_eq!(out, "");
    assert!(err.contains("--json-facts"), "{err}");
}
