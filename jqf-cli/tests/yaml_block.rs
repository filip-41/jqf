//! `yaml.block@1` (D4): the human-readable YAML dialect, at the product path.
//!
//! The canonical dialects answer a machine and are pinned byte-for-byte in `tagged_yaml.rs` and the codec smoke
//! harnesses. This file pins the dialect a PERSON sees: what `--output-format yaml` emits when no dialect is named.
//!
//! The laws pinned here:
//!
//! * the yq block shape — block maps, sequence items at a two-space indent
//!   under their key, `---` BETWEEN documents and no `...` terminator;
//! * the quoting rule — plain wherever the text reads back as itself, quoted
//!   wherever a reader would resolve it into something else, literal block scalars for clean multiline text;
//! * key order is the value's order, never sorted;
//! * composition with the encode-projection policy — tags emit NATIVELY and
//!   silently because YAML has tags, while temporals and byte strings still project and still warn;
//! * the round trip: what the block dialect writes, this codec reads back to
//!   the same value.
//!
//! `tools/jqf-cli-jq-compat.sh` cannot carry any of it: jq neither reads nor writes YAML, so there is no oracle.

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
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stdout).expect("stdout is UTF-8"),
        String::from_utf8(output.stderr).expect("stderr is UTF-8"),
    )
}

/// `jqf --output-format yaml <program>` with no dialect named.
fn block(program: &str, stdin: &str) -> (i32, String, String) {
    run(&["--output-format", "yaml", program], stdin)
}

/// `run` with raw byte stdin (an ill-formed UTF-8 source cannot be spelled as a Rust `&str`).
fn run_bytes(args: &[&str], stdin: &[u8]) -> (i32, String, String) {
    let mut child = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("jqf spawns");
    if let Err(error) = child.stdin.take().expect("stdin is piped").write_all(stdin) {
        assert!(
            error.kind() == std::io::ErrorKind::BrokenPipe,
            "input writes to jqf's stdin: {error}"
        );
    }
    let output = child.wait_with_output().expect("jqf runs to completion");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stdout).expect("stdout is UTF-8"),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn the_default_yaml_output_is_the_block_shape() {
    // D4's acceptance probe, byte for byte.
    let (code, out, err) = block(
        ".",
        r#"{"name":"ann","tags":["a","b"],"meta":{"age":30,"ok":true,"note":null},"pi":3.14}"#,
    );
    assert_eq!(code, 0);
    assert_eq!(err, "");
    assert_eq!(
        out,
        concat!(
            "name: ann\n",
            "tags:\n",
            "  - a\n",
            "  - b\n",
            "meta:\n",
            "  age: 30\n",
            "  ok: true\n",
            "  note: null\n",
            "pi: 3.14\n",
        )
    );
}

#[test]
fn naming_a_canonical_dialect_still_reaches_it() {
    // The two canonical dialects did not move; they got a name.
    let (code, out, _) = run(
        &[
            "--output-format",
            "yaml",
            "--output-dialect",
            "yaml.single-document@1",
            ".",
        ],
        r#"{"a":1}"#,
    );
    assert_eq!(code, 0);
    assert_eq!(out, "!!map {\n  ? !!str \"a\"\n  : !!int \"1\",\n}\n\n");
}

#[test]
fn documents_are_separated_not_prefixed() {
    // yq's stream shape: no marker before the first document, `---` between each pair, no `...` anywhere.
    let (code, out, _) = block(".", "{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n");
    assert_eq!(code, 0);
    assert_eq!(out, "a: 1\n---\nb: 2\n---\nc: 3\n");
}

#[test]
fn nested_collections_indent_under_their_introducer() {
    let (code, out, _) = block(
        ".",
        r#"{"a":[[1,2],[3]],"b":{"c":{"d":[{"e":1}]}},"empty_list":[],"empty_map":{}}"#,
    );
    assert_eq!(code, 0);
    assert_eq!(
        out,
        concat!(
            "a:\n",
            "  - - 1\n",
            "    - 2\n",
            "  - - 3\n",
            "b:\n",
            "  c:\n",
            "    d:\n",
            "      - e: 1\n",
            "empty_list: []\n",
            "empty_map: {}\n",
        )
    );
}

#[test]
fn a_string_is_quoted_exactly_when_it_would_not_read_back_as_itself() {
    let (code, out, _) = block(
        ".",
        concat!(
            r##"{"plain":"plain text","bool_word":"yes","numberish":"3.5","empty":"","##,
            r##""colon":"has: colon","trailing":"trail ","hash":"#hash","inline_hash":"a #b","##,
            r##""dated":"2026-06-15","dash":"-ann","item":"- ann","key_shaped":"key:","##,
            r##""unicode":"café","hex":"0x1f"}"##
        ),
    );
    assert_eq!(code, 0);
    assert_eq!(
        out,
        concat!(
            "plain: plain text\n",
            "bool_word: \"yes\"\n",
            "numberish: \"3.5\"\n",
            "empty: \"\"\n",
            "colon: \"has: colon\"\n",
            "trailing: \"trail \"\n",
            "hash: \"#hash\"\n",
            "inline_hash: \"a #b\"\n",
            "dated: \"2026-06-15\"\n",
            "dash: -ann\n",
            "item: \"- ann\"\n",
            "key_shaped: \"key:\"\n",
            "unicode: café\n",
            "hex: \"0x1f\"\n",
        )
    );
}

#[test]
fn clean_multiline_text_uses_a_literal_block_scalar() {
    // `|` keeps the one trailing newline, `|-` marks content that has none. Text the literal form cannot carry byte for
    // byte — a tab, a blank final line — is quoted instead, where every byte is explicit.
    let (code, out, _) = block(
        ".",
        r#"{"clip":"line1\nline2\n","strip":"line1\nline2","tabbed":"a\n\tb\n","double":"a\nb\n\n"}"#,
    );
    assert_eq!(code, 0);
    assert_eq!(
        out,
        concat!(
            "clip: |\n",
            "  line1\n",
            "  line2\n",
            "strip: |-\n",
            "  line1\n",
            "  line2\n",
            "tabbed: \"a\\n\\tb\\n\"\n",
            "double: \"a\\nb\\n\\n\"\n",
        )
    );
}

#[test]
fn a_key_that_would_re_resolve_is_quoted_and_order_is_never_sorted() {
    let (code, out, _) = block(".", r#"{"zeta":1,"n":2,"alpha":3,"2026-06-15":4}"#);
    assert_eq!(code, 0);
    assert_eq!(out, "zeta: 1\n\"n\": 2\nalpha: 3\n\"2026-06-15\": 4\n");
}

#[test]
fn a_tag_emits_natively_and_says_nothing() {
    // The composition law: YAML spells tags, so nothing is projected here and nothing is warned about — unlike the same
    // value on the way to JSON.
    let (code, out, err) = run(
        &["--input-format", "yaml", "--output-format", "yaml", "."],
        "v: !money 12.5\n",
    );
    assert_eq!(code, 0);
    assert_eq!(out, "v: !money 12.5\n");
    assert_eq!(err, "", "a native spelling is not a projection");
}

#[test]
fn a_tagged_collection_opens_on_the_line_after_its_tag() {
    // A tag property and a block collection cannot share a line.
    let (code, out, err) = run(
        &["--input-format", "yaml", "--output-format", "yaml", "."],
        "v: !list\n  - 1\n  - 2\n",
    );
    assert_eq!(code, 0);
    assert_eq!(out, "v: !list\n  - 1\n  - 2\n");
    assert_eq!(err, "");
}

#[test]
fn a_temporal_still_projects_and_still_warns() {
    // YAML's core schema has no timestamp, so the projection stands — and the RFC 3339 text is QUOTED, because unquoted
    // it is the one thing a reader that DOES carry the type would resolve back into something else.
    let (code, out, err) = run(
        &["--input-format", "toml", "--output-format", "yaml", "."],
        "d = 2026-06-15\nt = 12:34:56\n",
    );
    assert_eq!(code, 0);
    assert_eq!(out, "d: \"2026-06-15\"\nt: \"12:34:56\"\n");
    assert_eq!(
        err,
        "jqf: warning: 2 datetimes rendered as RFC 3339 strings; \
         --types-as-strings reads temporals as their plain text\n"
    );
}

#[test]
fn a_byte_string_still_projects_to_base64url_and_still_warns() {
    // `!!binary` is a spelling this codec decodes to a non-core TAGGED value rather than to `Bytes`, so emitting it
    // would not round-trip to what was read. Both YAML dialects therefore make the same choice JSON does. `Ehello` is
    // CBOR for the five-byte string `hello`.
    let (code, out, err) = run(&["--input-format", "cbor", "--output-format", "yaml", "."], "Ehello");
    assert_eq!(code, 0);
    assert_eq!(out, "aGVsbG8\n");
    assert_eq!(err, "jqf: warning: 1 byte string rendered as base64url text\n");
}

#[test]
fn what_the_block_dialect_writes_this_codec_reads_back() {
    // The round trip is the quoting rule's whole justification, so it is the thing worth pinning: block bytes back
    // through the YAML reader must land on the value that produced them.
    const VALUE: &str = concat!(
        r#"{"plain":"plain text","bool_word":"yes","numberish":"3.5","empty":"","#,
        r#""colon":"has: colon","clip":"line1\nline2\n","strip":"a\nb","#,
        r#""dated":"2026-06-15","dash":"-ann","nested":[[1,2],{"k":null}],"#,
        r#""empty_list":[],"empty_map":{},"pi":3.14,"flag":false}"#
    );
    let (code, block_bytes, _) = block(".", VALUE);
    assert_eq!(code, 0);
    let (code, round_tripped, _) = run(&["--input-format", "yaml", "-c", "."], &block_bytes);
    assert_eq!(code, 0);
    let (code, original, _) = run(&["-c", "."], VALUE);
    assert_eq!(code, 0);
    assert_eq!(round_tripped, original);
}

#[test]
fn a_yaml_identity_pipeline_keeps_the_file_comments() {
    // The consolidation thesis's sharpest test: a YAML-to-YAML pipeline that deletes every comment is a config
    // destroyer, whatever else it preserves. Leading comments ride `yaml.comment@1` facts on the located document and
    // the block encoder re-emits them ahead of their owner; a document-trailer comment rides the ROOT's
    // `yaml.comment_foot@1` and re-emits after the body. The same-line comment `# about the name` is, under 144 D2,
    // `name`'s INLINE comment (`yaml.comment_inline@1`) — it no longer leaks into `port`'s leading list, and since 144
    // S5 the encoder re-emits it after the value on the same line (the old expectation dropped it; the old text that
    // moved it before `port` was the wrong-attribution artifact this plan exists to remove).
    let (code, out, err) = run(
        &["--input-format", "yaml", "--output-format", "yaml", "."],
        "# top comment\nname: app   # about the name\nport: 8080\nlist:\n  - a\n  # about b\n  - b\n# trailer\n",
    );
    assert_eq!(code, 0);
    assert_eq!(err, "");
    assert_eq!(
        out,
        "# top comment\nname: app # about the name\nport: 8080\nlist:\n  - a\n  # about b\n  - b\n# trailer\n"
    );
}

#[test]
fn a_yaml_projection_keeps_the_subtree_comments_it_can_see() {
    // A scoped read serves a subtree; comments owned by nodes inside it re-emit, and comments outside it are simply not
    // part of the answer.
    let (code, out, _) = run(
        &["--input-format", "yaml", "--output-format", "yaml", ".list"],
        "name: app\nlist:\n  # first\n  - a\n  - b\n",
    );
    assert_eq!(code, 0);
    assert_eq!(out, "# first\n- a\n- b\n");
}

#[test]
fn an_anchored_document_survives_the_floor() {
    // 's prerequisite (lane C4): a floor (whole-document re-encode) must round-trip an anchored document instead of
    // silently flattening it — the hole that exists today independently of any fact, because `block.rs` had no anchor
    // emission at all. The authored name rides a `yaml.anchor@1` fact from decode; the block renderer emits `&name` at
    // the anchor's own position and `*name` at every later occurrence of the shared document node (the walk shares ONE
    // node across an anchor and its aliases), so the alias site names the anchor instead of duplicating its value.
    let (code, out, err) = run(
        &["--input-format", "yaml", "--output-format", "yaml", "."],
        "base: &x {n: 1}\na: *x\nb: *x\n",
    );
    assert_eq!(code, 0);
    assert_eq!(err, "");
    assert!(out.contains("base: &x"), "anchor survives, got: {out}");
    assert!(out.contains("a: *x"), "alias survives, got: {out}");
    assert!(out.contains("b: *x"), "alias survives, got: {out}");
    // The emitted bytes re-decode to the same document and re-emit themselves: the round trip is a fixpoint.
    let (code, second, err2) = run(&["--input-format", "yaml", "--output-format", "yaml", "."], &out);
    assert_eq!(code, 0);
    assert_eq!(err2, "");
    assert_eq!(second, out, "re-encode of the anchor output must be stable");
}

#[test]
fn a_scalar_anchor_and_a_rebound_name_round_trip() {
    // A scalar anchor emits inline (`a: &v 1`); a REBOUND name — two `&x` anchors, the second winning the alias — keeps
    // both authored anchors and resolves the alias to the second, exactly as the source did.
    let (code, out, _) = run(
        &["--input-format", "yaml", "--output-format", "yaml", "."],
        "a: &v 1\nb: *v\n",
    );
    assert_eq!(code, 0);
    assert_eq!(out, "a: &v 1\nb: *v\n");
    let (code, out, _) = run(
        &["--input-format", "yaml", "--output-format", "yaml", "."],
        "a: &x 1\nb: &x 2\nc: *x\n",
    );
    assert_eq!(code, 0);
    assert_eq!(out, "a: &x 1\nb: &x 2\nc: *x\n");
}

#[test]
fn a_merge_keyed_document_keeps_its_anchor() {
    // The merge-expansion law (§3.3): decode splices the anchored mapping's entries into the host mapping by reusing
    // the source node ids, so `svc_a.timeout` and `defaults.timeout` are one document node. The merged members still
    // flatten (a merge-key local override is plan 142 W1, not this lane), but the anchor itself now SURVIVES the floor
    // instead of being silently dropped.
    let (code, out, _) = run(
        &["--input-format", "yaml", "--output-format", "yaml", "."],
        "defaults: &defaults\n  timeout: 30\n  retries: 3\nsvc_a:\n  <<: *defaults\n  name: a\n",
    );
    assert_eq!(code, 0);
    assert!(out.contains("defaults: &defaults"), "anchor survives, got: {out}");
    assert!(out.contains("timeout: 30"), "merged member survives, got: {out}");
}

#[test]
fn an_anchored_tagged_value_emits_anchor_before_tag() {
    // Both properties render on the introducing line, anchor first: `&m` then `!money`, matching the property order
    // this codec's own decoder accepts (an alias site then names the anchor alone).
    let (code, out, _) = run(
        &["--input-format", "yaml", "--output-format", "yaml", "."],
        "v: &m !money 12.5\nw: *m\n",
    );
    assert_eq!(code, 0);
    assert!(out.contains("v: &m !money 12.5"), "got: {out}");
    assert!(out.contains("w: *m"), "got: {out}");
}

#[test]
fn an_ill_formed_yaml_source_refuses_with_the_offset_named() {
    // The never-silently-mangled law for a byte-sourced format: an ill-formed scalar must REFUSE loudly with the offset
    // named, never silently become `null` (`port: \x80\x81`) or truncate at the first invalid byte (`msg:
    // hello\x80world` → `"hello"`). yq refuses the same files loudly; jqf's refusal is the codec class (exit 5), and
    // `--edit` on such a file is the same clean refusal — never the internal contract violation the mangled decode used
    // to produce.
    let (code, out, err) = run_bytes(&["--input-format", "yaml", "."], b"port: \x80\x81\n");
    assert_eq!(code, 5, "an ill-formed scalar is a decode refusal");
    assert_eq!(out, "");
    assert!(
        err.contains("invalid-utf8") && err.contains('6'),
        "the refusal must name the offset, got: {err}"
    );
    let (code, out, err) = run_bytes(&["--input-format", "yaml", "."], b"msg: hello\x80world\n");
    assert_eq!(code, 5);
    assert_eq!(out, "");
    assert!(
        err.contains("invalid-utf8") && err.contains('1') && err.contains('0'),
        "the refusal must name the mid-string offset, got: {err}"
    );
    // `--edit` gets the same refusal, not an internal contract violation.
    let (code, out, err) = run_bytes(
        &["--edit", "--input-format", "yaml", ".port = 9090"],
        b"port: \x80\x81\n",
    );
    assert_eq!(code, 5, "--edit on an ill-formed source is a decode refusal");
    assert_eq!(out, "");
    assert!(
        !err.contains("internal contract violation"),
        "no contract violation: {err}"
    );
    // A UTF-8 BOM and a valid UTF-16 source are still served.
    let (code, out, _) = run(&["--input-format", "yaml", ".a"], "\u{feff}a: 1\n");
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "1");
}
