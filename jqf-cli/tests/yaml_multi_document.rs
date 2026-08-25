//! Findings 13/14 (plan `.plans/037-probe-sweep-2026-08-03.md`): a multi-document YAML stream must publish every
//! document, and `-s`/`-n` must not leak the adjacent-value route's internal contract error.
//!
//! Before this fix, `YamlParseState` already yielded every document at the codec level (commit 5b3a7e11b), but the
//! CLI/SDK only ever drove the whole-document route through a single `poll` (via `ErasedAccessSession`, which is
//! deliberately single-outcome by contract), so every document after the first was silently dropped — exit 0, no
//! stderr. `-s`/`-n` took a different, adjacent-value-only path (`decode_eager_sequence`) that YAML's decoder
//! unconditionally rejected, surfacing as `InternalContractViolation { contract: "adjacent-value route opened without
//! reporting a consumed offset" }`.
//!
//! The fix makes YAML report a per-document `consumed_offset` (like JSON's adjacent-value mode already does), so the
//! SDK's existing, format-neutral reopen-at-offset sequence drive (`execute_sequence`, `decode_eager_sequence`,
//! `execute_source_edit`, …) handles multi-document YAML the same way it already handles adjacent JSON values. `-s`
//! therefore means what it means for JSON: collect every document into one array. There is no YAML-specific branch in
//! the CLI — the fix lives in the codec's decode layer, not here.
//!
//! Testing that fix surfaced a second, deeper instance of the same defect shape: jqf once served some programs through
//! single-shot NATIVE fast lanes — shallow-structure (`type`/`keys`), element-stream (`.[]`), structure-count
//! (`length`), and projected-stream (a construction over a streamed projection) — each of which validated and navigated
//! exactly ONE document before publishing, silently answering from the first document only over a multi-document stream
//! (exit 0, no error — the worst shape, since nothing signals data was dropped). Those specialized light routes are all
//! deleted; a multi-document input is served by the sequence path, one item per `---` unit, so every document is walked
//! in source order. These tests pin that walk from the CLI: `jqf --explain` reports `route: sequence` for a
//! multi-document input.
//!
//! No jq oracle exists for YAML (jq reads no YAML), so these are pinned against `yq` 4.53.2 and `PyYAML` 6.0.3 by hand
//! in the fix's commit message and report, not through `tools/jqf-cli-jq-compat.sh`.

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

fn stdout(args: &[&str], stdin: &str) -> String {
    let (code, out, err) = run(args, stdin);
    assert_eq!(
        code,
        0,
        "expected success for {args:?}, got {code}; stderr={}",
        String::from_utf8_lossy(&err)
    );
    String::from_utf8(out).expect("stdout is UTF-8")
}

#[test]
fn multi_document_default_route_emits_one_value_per_document() {
    // Finding 13's exact repro: three `---`-separated documents must each reach the output, not just the first.
    let out = stdout(
        &["--input-format", "yaml", "-c", "."],
        "---\na: 1\n---\na: 2\n---\na: 3\n",
    );
    assert_eq!(out, "{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n");
}

#[test]
fn multi_document_stream_with_explicit_end_markers_emits_every_document() {
    // Explicit `...` end markers between documents are a distinct boundary shape from a bare `---`; both must drive the
    // same reopen-at-offset sequence.
    let out = stdout(
        &["--input-format", "yaml", "-c", "."],
        "--- \na: 1\n...\n--- \na: 2\n...\n",
    );
    assert_eq!(out, "{\"a\":1}\n{\"a\":2}\n");
}

#[test]
fn single_document_yaml_still_decodes_without_a_stream_marker() {
    // Regression guard: a plain single document (no `---` at all) must keep working once YAML stops being treated as a
    // `single_document` format.
    let out = stdout(&["--input-format", "yaml", "-c", "."], "a: 1\nb: 2\n");
    assert_eq!(out, "{\"a\":1,\"b\":2}\n");
}

#[test]
fn slurp_collects_every_document_into_one_array() {
    // Finding 14: `-s` over YAML must mean what it means for JSON's adjacent-value family — collect every decoded value
    // into one array — not leak the internal-contract error.
    let out = stdout(
        &["--input-format", "yaml", "-sc", "."],
        "---\na: 1\n---\na: 2\n---\na: 3\n",
    );
    assert_eq!(out, "[{\"a\":1},{\"a\":2},{\"a\":3}]\n");
}

#[test]
fn null_input_inputs_walks_every_document_without_internal_contract_violation() {
    // Finding 14's `-n` half: `inputs` must be able to pull each document in turn, the same shape as JSON's
    // adjacent-value `-n 'inputs'`.
    let out = stdout(&["--input-format", "yaml", "-nc", "inputs"], "---\na: 1\n---\na: 2\n");
    assert_eq!(out, "{\"a\":1}\n{\"a\":2}\n");
}

#[test]
fn slurp_over_a_single_document_yaml_stream_still_wraps_in_an_array() {
    // A single-document stream is a one-element adjacent-value stream, not a special case: `-s` must still wrap it.
    let out = stdout(&["--input-format", "yaml", "-sc", "."], "a: 1\n");
    assert_eq!(out, "[{\"a\":1}]\n");
}

#[test]
fn multi_document_pushdown_program_walks_every_document_too() {
    // Not just plain `.`: a program that pushes down to one path (`.a`) resolves to a DIFFERENT codec route (the native
    // "Located"/scoped session) than the whole-document route bare `.` uses. That route needed the identical
    // `consumed_offset` fix — this pins it from the CLI so a future change to either route can't silently regress the
    // other back to "first document only".
    let out = stdout(
        &["--input-format", "yaml", "-c", ".a"],
        "---\na: 1\n---\na: 2\n---\na: 3\n",
    );
    assert_eq!(out, "1\n2\n3\n");
}

// Beyond the plain whole-document (`.`) and pushdown (`.a`) routes above, these four tests pin the same law for
// programs whose demand shapes once had dedicated fast lanes: `type`/`keys`, `.[]`, `length`, and a construction over a
// streamed projection. Each shape originally SILENTLY DROPPED every document after the first over a multi-document
// stream — exit 0, no error, worse than Finding 14's leaked `InternalContractViolation`, because nothing signaled that
// anything was wrong. The fix made each of those lanes DECLINE (an ordinary codec error, raised before anything is
// published) as soon as it detected a second document, so the SDK fell back to the sequence path that already walks
// every document correctly; those lanes have since been deleted from the access inventory (two slots plus hints), so
// these programs now serve through fallback paths directly. The assertion is unchanged either way: every YAML document
// in the stream is walked.

#[test]
fn multi_document_type_fallback_walks_every_document() {
    // A second YAML document must be walked, not truncated: both documents' types are published.
    let out = stdout(&["--input-format", "yaml", "-c", "type"], "---\na: 1\n---\na: 2\n");
    assert_eq!(out, "\"object\"\n\"object\"\n");
}

#[test]
fn multi_document_iterate_program_walks_every_document() {
    // `.[]` over a root array publishes every element of every document; a second document must not be truncated.
    let out = stdout(
        &["--input-format", "yaml", "-c", ".[]"],
        "---\n- 1\n- 2\n---\n- 3\n- 4\n",
    );
    assert_eq!(out, "1\n2\n3\n4\n");
}

#[test]
fn multi_document_length_program_walks_every_document() {
    // `length` over a root array answers per document; a second document must not be truncated.
    let out = stdout(
        &["--input-format", "yaml", "-c", "length"],
        "---\n- 1\n- 2\n---\n- 3\n- 4\n",
    );
    assert_eq!(out, "2\n2\n");
}

#[test]
fn multi_document_projected_construction_walks_every_document() {
    // `[.[] | {a}]` builds one projection per document; a second document must not be truncated.
    let out = stdout(
        &["--input-format", "yaml", "-c", "[.[] | {a}]"],
        "---\n- {a: 1, b: 2}\n---\n- {a: 3, b: 4}\n",
    );
    assert_eq!(out, "[{\"a\":1}]\n[{\"a\":3}]\n");
}

/// The empty-stream shapes are frozen (the recorded narrowing named in `YamlParseState`'s no-document arm): a
/// blank-only source is skipped by the adjacent-value drive's separator scan before the session opens, so it emits ZERO
/// items (jq's own empty-input behavior); a comment-only source opens the session, and the drive's outcome contract
/// forces ONE published document — the null scalar, the same value an explicit `---` yields.
#[test]
fn blank_only_stream_emits_zero_items_comment_only_emits_one_null() {
    let (code, out, _) = run(&["--input-format", "yaml", "-c", "."], "\n");
    assert_eq!(code, 0);
    assert_eq!(out, b"", "blank-only YAML emits zero items");

    let out = stdout(&["--input-format", "yaml", "-c", "."], "# c\n");
    assert_eq!(out, "null\n", "comment-only YAML publishes one null document");

    let out = stdout(&["--input-format", "yaml", "-c", "."], "---\n");
    assert_eq!(out, "null\n", "an explicit empty document is one null");
}

// --- syntax165 T6: one item per `---` unit through the edit lane ---------

#[test]
fn bare_scalar_stream_prints_one_item_per_unit() {
    // The plan's pin, verbatim shape: a two-unit stream of bare scalars prints one item per unit in source order.
    let out = stdout(&["--input-format", "yaml", "."], "1\n---\n2\n");
    assert_eq!(out, "1\n2\n");
}

#[test]
fn single_document_output_dialect_yields_one_encoded_item_per_unit() {
    // syntax165 T6, the output half: a single-document OUTPUT dialect over N input units yields N items, EACH encoded
    // as one single document — matching JSON's behavior (one encoded document per item), never one merged document.
    let out = stdout(
        &[
            "--input-format",
            "yaml",
            "--output-format",
            "yaml",
            "--output-dialect",
            "yaml.single-document@1",
            ".",
        ],
        "1\n---\n2\n",
    );
    // Two items, each ONE single document (the dialect's canonical scalar spelling plus its own trailing newline and
    // the facade's item newline): never one merged document.
    assert_eq!(out, "!!int \"1\"\n\n!!int \"2\"\n\n");
}

#[test]
fn edit_on_a_two_document_file_does_not_merge_the_units() {
    // The merge-splice repro (syntax165 T6): deleting doc 2's only member is a structural change whose floor re-encode
    // renders only the value — the unit's own `---` marker must survive it, or the published bytes read as ONE merged
    // document (`a: 1\n{}`).
    let out = stdout(&["--input-format", "yaml", "--edit", "del(.b)"], "a: 1\n---\nb: 2\n");
    assert_eq!(out, "a: 1\n---\n{}\n");
    // And the bytes read back as the same two-document stream.
    let out = stdout(&["--input-format", "yaml", "-c", "."], &out);
    assert_eq!(out, "{\"a\":1}\n{}\n");
}

#[test]
fn edit_in_place_splices_each_unit_into_the_original_file() {
    // `--edit --in-place` publishes per document into the original file: one splice per unit, every unit's `---`
    // framing intact.
    let dir = std::env::temp_dir().join(format!("jqf-t6-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("two.yaml");
    std::fs::write(&path, b"a: 1\n---\nb: 2\n").expect("fixture");
    let path_string = path.to_string_lossy().into_owned();
    let (code, out, err) = run(&["--edit", "--in-place", "del(.b)", &path_string], "");
    assert_eq!(
        code,
        0,
        "in-place edit exits 0; stderr={}",
        String::from_utf8_lossy(&err)
    );
    assert!(out.is_empty(), "the edited bytes go to the file, not stdout");
    let edited = std::fs::read(&path).expect("edited file");
    assert_eq!(edited, b"a: 1\n---\n{}\n", "one splice per unit, markers intact");
    let _ = std::fs::remove_dir_all(&dir);
}

// --- syntax165 T6 follow-up: the FIRST unit's framing survives a floor ----

#[test]
fn edit_keeps_the_first_document_leading_comments_across_a_floor() {
    // Doc 1's header comment lives in its own segment before the root value's span; the structural floor re-encode must
    // republish it exactly as it republishes a follower's `---` (the pre-fix lane dropped it).
    let out = stdout(
        &["--input-format", "yaml", "--edit", "del(.a)"],
        "# header comment\na: 1\n---\nb: 2\n",
    );
    assert_eq!(out, "# header comment\n{}\n---\nb: 2\n");
}

#[test]
fn edit_keeps_an_authored_leading_marker_on_the_first_unit() {
    // An AUTHORED leading `---` on the first unit survives a structural floor: the edited bytes still read back as the
    // same two documents.
    let out = stdout(
        &["--input-format", "yaml", "--edit", "del(.a)"],
        "---\na: 1\n---\nb: 2\n",
    );
    assert_eq!(out, "---\n{}\n---\nb: 2\n");
    let reread = stdout(&["--input-format", "yaml", "-c", "."], &out);
    assert_eq!(reread, "{}\n{\"b\":2}\n");
}

#[test]
fn edit_patch_path_stays_byte_identical_with_a_first_unit_prefix() {
    // The patch path never needed the prefix rule and must not gain one: an append patch over both units publishes the
    // same bytes the lane published before the first-unit law existed.
    let out = stdout(
        &["--input-format", "yaml", "--edit", ".c = 3"],
        "# header comment\na: 1\n---\nb: 2\n",
    );
    assert_eq!(out, "# header comment\na: 1\nc: 3\n---\nb: 2\nc: 3\n");
}

#[test]
fn edit_of_an_empty_or_comment_only_first_unit_refuses_at_document_authority() {
    // The decided empty-first-unit law (§4.8): an explicit `---` or a comment-only unit publishes the parser's bare
    // empty document, which carries NO committed spans — so there is no framable prefix and, per the
    // separately-recorded defect this lane leaves alone, no located document authority either: the edit refuses at exit
    // 5 rather than silently re-framing. When that authority defect is fixed, this pin flips to the no-span/no-prefix
    // floor law (`x: 1` with no framing).
    for input in ["---\n", "# c\n"] {
        let (code, out, err) = run(&["--input-format", "yaml", "--edit", ".x = 1"], input);
        assert_eq!(code, 5, "empty first unit edit exits 5: {out:?} {err:?}");
        assert!(out.is_empty(), "nothing publishes: {out:?}");
        assert!(
            String::from_utf8_lossy(&err).contains("edit lane document authority"),
            "the refusal names the authority contract: {err:?}"
        );
    }
}
