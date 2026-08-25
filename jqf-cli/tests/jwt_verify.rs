//! The JWT HS256 verification probe — the +021 ruling's pinned acceptance test.
//!
//! The ruling: "verify a real JWT's signature entirely in jqf". A JWT's signature is HMAC-SHA256 of the signing input
//! (header.payload, base64url) keyed by the shared secret, itself base64url — so the probe spans BOTH plans: 's
//! `hmac_sha256_base64url` (the paired digest form, chosen because digest bytes are not UTF-8-safe through a string
//! round-trip) and 's `base64url_encode`/`base64url_decode` when a token is BUILT rather than verified.
//!
//! The vector is the canonical jwt.io example: header `{"alg":"HS256", "typ":"JWT"}`, payload
//! `{"sub":"1234567890","name":"John Doe", "iat":1516239022}`, secret "your-256-bit-secret". Its signature
//! (`SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c`) was re-derived independently with python3 (hmac + hashlib + base64)
//! at pinning time; the same value is pinned as corpus rows in `tools/jqf-cli-jq-compat.sh` (the `codec` kind), so a
//! drift here or there fails loudly.

use std::io::Write as _;
use std::process::{Command, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// Runs `jqf args…` with `stdin` as the input, returning (exit code, stdout).
fn run(args: &[&str], stdin: &str) -> (i32, String) {
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
        .write_all(stdin.as_bytes())
        .expect("input writes to jqf's stdin");
    let output = child.wait_with_output().expect("jqf runs to completion");
    assert!(
        output.status.success(),
        "jqf failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stdout).expect("stdout is UTF-8"),
    )
}

/// The canonical jwt.io HS256 token (header.payload.signature).
///
/// The token as a JSON string literal, for stdin (a JWT is not itself JSON).
const JWT_INPUT: &str = "\"eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c\"";

const SECRET: &str = "your-256-bit-secret";

#[test]
fn a_real_hs256_signature_verifies_in_pure_jqf() {
    // Recomputed in jqf: split the token, recompute HMAC-SHA256 of header.payload with the secret (base64url), compare
    // to the third segment. No decode step is needed — the *_base64url digest form is exactly the JWT spelling.
    let program = format!(
        "split(\".\") as $p | \
         (($p[0] + \".\" + $p[1]) | hmac_sha256_base64url(\"{SECRET}\")) as $sig | \
         $p[2] == $sig"
    );
    let (code, out) = run(&["-c", &program], JWT_INPUT);
    assert_eq!(code, 0);
    assert_eq!(out, "true\n");
}

#[test]
fn an_altered_signing_input_fails_verification() {
    // One extra byte in the signing input changes the recomputed signature.
    let program = format!(
        "split(\".\") as $p | \
         (($p[0] + \".\" + $p[1] + \"x\") | hmac_sha256_base64url(\"{SECRET}\")) as $sig | \
         $p[2] == $sig"
    );
    let (code, out) = run(&["-c", &program], JWT_INPUT);
    assert_eq!(code, 0);
    assert_eq!(out, "false\n");
}

#[test]
fn the_token_can_be_built_from_scratch_with_the_020_family() {
    // The other direction of the probe: BUILD the token from the header and payload objects with base64url_encode and
    // sign it with hmac_sha256_base64url — the recomputed signature must equal the pinned one, byte for byte.
    let program = format!(
        "{{\"alg\":\"HS256\",\"typ\":\"JWT\"}} as $h | \
         {{\"sub\":\"1234567890\",\"name\":\"John Doe\",\"iat\":1516239022}} as $p | \
         ($h | tostring | base64url_encode) as $hb | \
         ($p | tostring | base64url_encode) as $pb | \
         (($hb + \".\" + $pb) | hmac_sha256_base64url(\"{SECRET}\")) as $sig | \
         [$hb, $pb, $sig]"
    );
    let (code, out) = run(&["-c", &program], "null");
    assert_eq!(code, 0);
    assert_eq!(
        out,
        "[\"eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9\",\"eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ\",\"SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c\"]\n"
    );
}

#[test]
fn the_header_and_payload_segments_decode_back_to_json() {
    // The 020 half of the composition, read side: the header and payload segments ARE valid UTF-8 JSON, so
    // base64url_decode recovers them exactly. (Digest bytes are NOT — which is why the *_base64url paired forms exist
    // rather than a hex+decode pipeline.)
    let program = "split(\".\") as $p | \
         (($p[0] | base64url_decode) | fromjson) as $h | \
         (($p[1] | base64url_decode) | fromjson) as $pl | \
         [$h, $pl]";
    let (code, out) = run(&["-c", program], JWT_INPUT);
    assert_eq!(code, 0);
    assert_eq!(
        out,
        "[{\"alg\":\"HS256\",\"typ\":\"JWT\"},{\"sub\":\"1234567890\",\"name\":\"John Doe\",\"iat\":1516239022}]\n"
    );
}
