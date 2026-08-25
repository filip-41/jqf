//! The 043 W12 tag-LAYER reads, pinned at the product path (brief 6).
//!
//! A located read that does not descend a tag LAYER answers about the tag, not the value. The layer shape is CBOR's: an
//! uninterpreted tag builds jqf-data's kindless `Unrepresentable` node whose only occurrence is the payload, so a
//! payload-transparent read (`kind`, scalar, container projection) MUST descend before it asks the node what it is.
//! YAML records its tag ON the value node, so it never exercises the layer; every row here feeds CBOR bytes through a
//! `.x` static prefix (the located route, where the tag node is the located input).
//!
//! Each row failed on the pre-fix binary (main at `b80cd8db0`) with an `internal contract violation` over a perfectly
//! valid document; the same programs answer about the payload's truth / number / member identities here. The
//! `tools/jqf-cli-jq-compat.sh` corpus cannot carry them: jq reads no CBOR and has no tag concept, so there is no
//! system-jq oracle.
//!
//! The sites pinned, one row each:
//!
//! * `select(.)` — `jqf-engine/src/semantics/truth.rs` (`is_truthy`);
//! * `finites` — `jqf-engine/src/registry/builtins/rider.rs`
//!   (`result_number_passes`);
//! * `has($k)` — `jqf-engine/src/registry/builtins/reshape.rs` (`has`);
//! * `sample(1)` — `jqf-engine/src/exec/mod.rs` (`eval_analytics` fast path,
//!   which pre-fix DECLINED the layer and post-fix engages its payload);
//! * `group_by(.)` — `jqf-engine/src/exec/mod.rs` (`located_child_count`);
//! * `{(.x): 1}` — `jqf-engine/src/exec/mod.rs` (`extract_key_string` — a
//!   tagged string stays a key MISMATCH, never a silently unwrapped key, and the mismatch now renders cleanly);
//! * `.[$k]` over a located tag-layer key — `jqf-engine/src/exec/message.rs`
//!   (`result_kind` / `write_result`, the operand rendering);
//! * `.[]` over a tag-layer scalar — the same `message.rs` writers on the
//!   iterate-mismatch operand;
//! * `length` on a byte string — the W12 rider taken: the byte count (the
//!   documented deferral closed for the one non-JSON kind with a natural length).

use std::io::Write as _;
use std::process::{Command, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// Runs `jqf --input-format cbor args…` with `stdin` bytes, returning (exit code, stdout, stderr).
fn run(args: &[&str], stdin: &[u8]) -> (i32, Vec<u8>, Vec<u8>) {
    let mut child = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(["--input-format", "cbor"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("jqf spawns");
    // A usage-error child exits WITHOUT reading stdin, closing the pipe mid-write; BrokenPipe is the expected race
    // there, not a test failure (surfaced by the 003 linux-amd64 emulated lane, where the child's exit reliably beats
    // the parent's write).
    if let Err(error) = child.stdin.take().expect("stdin is piped").write_all(stdin) {
        assert!(
            error.kind() == std::io::ErrorKind::BrokenPipe,
            "input writes to jqf's stdin: {error}"
        );
    }
    let output = child.wait_with_output().expect("jqf runs to completion");
    (output.status.code().unwrap_or(-1), output.stdout, output.stderr)
}

/// `{x: <value>}` in CBOR, built by hand: `a1 61 78` is the one-member map keyed `x`, then the value's own bytes.
fn map_x(value: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0xa1, 0x61, b'x'];
    bytes.extend_from_slice(value);
    bytes
}

/// CBOR tag 100 (`d8 64`) wrapping `payload` — an uninterpreted tag, so the decoder builds the kindless tag LAYER.
fn tag100(payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0xd8, 0x64];
    bytes.extend_from_slice(payload);
    bytes
}

#[test]
fn select_reads_the_payloads_truth_through_the_tag_layer() {
    // `{x: tag(false)} |.x | select(.)`: the tag node is the located input, and the truth check must descend to the
    // payload's `false` — nothing emitted, exit 0. Pre-fix: `internal contract violation: select truth check failed`.
    let (code, out, _) = run(&["-c", ".x | select(.)"], &map_x(&tag100(&[0xf4])));
    assert_eq!(code, 0, "a tagged false must be falsy");
    assert!(out.is_empty(), "select over a tagged false emits nothing");

    // The mirror: a tagged `true` is truthy and emits the value.
    let (code, out, _) = run(&["-c", ".x | select(.)"], &map_x(&tag100(&[0xf5])));
    assert_eq!(code, 0);
    assert_eq!(out, b"true\n");
}

#[test]
fn the_number_filter_reads_the_payload_through_the_tag_layer() {
    // `{x: tag(5)} |.x | finites`: the number filter must see the payload's number, so 5 passes. Pre-fix: `internal
    // contract violation: number filter scalar failed`.
    let (code, out, _) = run(&["-c", ".x | finites"], &map_x(&tag100(&[0x05])));
    assert_eq!(code, 0);
    assert_eq!(out, b"5\n");
}

#[test]
fn has_reads_the_payloads_member_identities_through_the_tag_layer() {
    // `{x: tag({a:1})} |.x | "a" as $k | has($k)`: the dynamic key forces the evaluator's located path (the shallow
    // route serves literal keys only), and the presence test must answer about the payload object's members. Pre-fix:
    // `internal contract violation: has evaluation over a valid located document failed`.
    let tagged_object = map_x(&tag100(&[0xa1, 0x61, 0x61, 0x01]));
    let (code, out, _) = run(&["-c", ".x | \"a\" as $k | has($k)"], &tagged_object);
    assert_eq!(code, 0);
    assert_eq!(out, b"true\n");

    let (code, out, _) = run(&["-c", ".x | \"b\" as $k | has($k)"], &tagged_object);
    assert_eq!(code, 0);
    assert_eq!(out, b"false\n");
}

#[test]
fn sample_engages_its_fast_path_over_a_tagged_array_payload() {
    // `{x: tag([1,2,3])} |.x | sample(1)` with a fixed seed: the small-draw fast path descends the layer to the payload
    // array and draws from it. Pre-fix the fast path declined the layer (the ordinary materializing path answered, so
    // this row pinned the bytes both ways); post-fix the fast path engages and must answer the SAME bytes as the owned
    // path.
    let tagged_array = map_x(&tag100(&[0x83, 0x01, 0x02, 0x03]));
    let (code, out, _) = run(&["--seed", "7", "-c", ".x | sample(1)"], &tagged_array);
    assert_eq!(code, 0);
    assert_eq!(out, b"[1]\n");

    // The owned twin (`[.]` materializes the layer) is the byte oracle for the fast path: the two must draw identically
    // under one seed.
    let (code, owned, _) = run(&["--seed", "7", "-c", ".x | [.] | .[0] | sample(1)"], &tagged_array);
    assert_eq!(code, 0);
    assert_eq!(out, owned, "the fast path and the materializing path draw identically");
}

#[test]
fn the_keyed_drive_counts_a_tagged_arrays_payload_children() {
    // `{x: tag([1,2,3])} |.x | group_by(.)`: the keyed drive's width probe must descend to the payload array. Pre-fix:
    // `internal contract violation: join container kind failed over a valid node`.
    let tagged_array = map_x(&tag100(&[0x83, 0x01, 0x02, 0x03]));
    let (code, out, _) = run(&["-c", ".x | group_by(.)"], &tagged_array);
    assert_eq!(code, 0);
    assert_eq!(out, b"[[1],[2],[3]]\n");
}

#[test]
fn a_tagged_string_dynamic_key_is_a_clean_mismatch_never_an_internal_error() {
    // `{x: tag("a")} | {(.x): 1}`: a non-core tagged string is NOT silently unwrapped into an object key (AGENTS.md),
    // so the construction refuses — and the refusal must be jq's clean key mismatch, not an internal contract
    // violation. Pre-fix: `internal contract violation: key node scalar view failed`.
    let (code, out, err) = run(&["-c", "{(.x): 1}"], &map_x(&tag100(&[0x61, b'a'])));
    assert_eq!(code, 5);
    assert!(out.is_empty(), "a refused key publishes nothing");
    let err = String::from_utf8(err).expect("stderr is UTF-8");
    assert!(
        err.contains("Cannot use string (\"a\") as object key"),
        "the refusal names the tagged payload, got: {err}"
    );
}

#[test]
fn a_located_tag_layer_key_renders_its_own_mismatch_message() {
    // `.. | select(type == "boolean") as $k |.[$k]` over `{x: tag(false)}`: the recursive descent binds the located
    // tag-layer node, and indexing with it is the boolean-key mismatch — whose operand rendering must descend the
    // layer. Pre-fix: `internal contract violation: operand rendering over a valid located document failed`.
    let (code, out, err) = run(
        &["-c", ".. | select(type == \"boolean\") as $k | .[$k]"],
        &map_x(&tag100(&[0xf4])),
    );
    assert_eq!(code, 5);
    assert!(out.is_empty());
    let err = String::from_utf8(err).expect("stderr is UTF-8");
    assert!(
        err.contains("Cannot index boolean with boolean (false)"),
        "the mismatch names the payload's kind and value, got: {err}"
    );
}

#[test]
fn an_iterate_mismatch_renders_a_tag_layer_operand_as_its_payload() {
    // `{x: tag(5)} |.x |.[]`: a tag-layer scalar is not iterable, and the iterate-mismatch operand must render the
    // payload's number. Pre-fix: `internal contract violation: operand rendering over a valid located document failed`.
    let (code, out, err) = run(&["-c", ".x | .[]"], &map_x(&tag100(&[0x05])));
    assert_eq!(code, 5);
    assert!(out.is_empty());
    let err = String::from_utf8(err).expect("stderr is UTF-8");
    assert!(
        err.contains("Cannot iterate over number (5)"),
        "the mismatch names the payload's number, got: {err}"
    );
}

#[test]
fn length_on_a_byte_string_counts_its_bytes() {
    // `{x: h'c2Fk'} |.x | length`: the W12 rider taken — a byte string's length is its byte count (4), closing the
    // deferral for the one non-JSON kind with a natural length. Pre-fix: exit 5 with the misleading `string (null) has
    // no length`.
    let (code, out, _) = run(&["-c", ".x | length"], &map_x(&[0x44, b'c', b'2', b'F', b'k']));
    assert_eq!(code, 0);
    assert_eq!(out, b"4\n");

    // The located and owned authorities agree: materializing through `[.]` answers the same count.
    let (code, owned, _) = run(
        &["-c", ".x | [.] | .[0] | length"],
        &map_x(&[0x44, b'c', b'2', b'F', b'k']),
    );
    assert_eq!(code, 0);
    assert_eq!(out, owned);
}
