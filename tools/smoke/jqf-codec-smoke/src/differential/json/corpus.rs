//! The Stage 0 differential corpus: embedded fixtures plus boundary accept
//! and reject cases, each paired with the verdict both jqf and `serde_json`
//! must agree on.
//!
//! There are deliberately NO byte-order-mark cases here. jqf's BOM law is
//! owned by the adjacent-value entry point (a source-start mark before the
//! first value, RFC 8259 §8.1); this oracle drives the whole-document route,
//! where a BOM stays rejected by both decoders. The adjacent law is covered by
//! the compat corpus's BOM rows and the codec's adjacent-path unit tests.

use std::fmt::Write as _;

/// Nesting-depth ceiling given to jqf's resource context for every corpus
/// case. Comfortably above [`BOUNDARY_NESTING_DEPTH`] and matches the
/// harness convention of 256. Production's own configured ceiling
/// (`jqf-cli/src/main.rs::MAX_NESTING_DEPTH`) is 1024; either would do here
/// since depth is never the limiting factor outside the dedicated boundary
/// case below.
pub(crate) const HARNESS_DEPTH_LIMIT: u32 = 256;

/// Container nesting used by the deep-nesting boundary-accept case.
///
/// Chosen to sit strictly under *both* effective limits: empirically
/// verified `serde_json` accepts at most 127 levels of array/object nesting
/// with its default (non-disabled) 128-frame recursion limit (128 fails,
/// 127 succeeds — see the campaign notes), and it sits far under
/// [`HARNESS_DEPTH_LIMIT`].
pub(crate) const BOUNDARY_NESTING_DEPTH: usize = 120;

/// What both decoders are expected to agree on for one corpus case.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Expect {
    /// Both decoders must accept, with equal semantic checksums.
    Accept,
    /// Both decoders must reject (error kinds need not match).
    Reject,
}

/// One named differential case.
pub(crate) struct Case {
    pub(crate) category: &'static str,
    pub(crate) name: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) expect: Expect,
    pub(crate) depth_limit: u32,
}

fn case(category: &'static str, name: impl Into<String>, bytes: Vec<u8>, expect: Expect) -> Case {
    Case {
        category,
        name: name.into(),
        bytes,
        expect,
        depth_limit: HARNESS_DEPTH_LIMIT,
    }
}

/// Builds the complete Stage 0 corpus.
pub(crate) fn build() -> Vec<Case> {
    let mut cases = Vec::new();
    cases.extend(bench_fixtures());
    cases.extend(boundary_accepts());
    cases.extend(rejects());
    cases
}

// --- bench fixtures ---------------------------------------------------
//
// Three generators (same shapes: a 512-item nested catalog, a 1024-entry
// escape-heavy array, and a 4096-write duplicate-key object).

fn bench_fixtures() -> Vec<Case> {
    vec![
        case(
            "fixture",
            "fixture/nested-catalog",
            nested_catalog().into_bytes(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/escape-array",
            escape_array().into_bytes(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/wide-duplicate-object",
            wide_duplicate_object().into_bytes(),
            Expect::Accept,
        ),
    ]
}

fn nested_catalog() -> String {
    let mut value = String::from("{\"meta\":{\"version\":1},\"catalog\":[");
    for index in 0..512 {
        if index != 0 {
            value.push(',');
        }
        let _ = write!(
            value,
            "{{\"id\":{index},\"name\":\"item-{index}\",\"price\":{}.25,\"active\":true}}",
            index + 10
        );
    }
    value.push_str("]}");
    value
}

fn escape_array() -> String {
    let mut value = String::from("[");
    for index in 0..1024 {
        if index != 0 {
            value.push(',');
        }
        value.push_str("\"line\\nquote\\\"slash\\\\music\\uD834\\uDD1E\"");
    }
    value.push(']');
    value
}

fn wide_duplicate_object() -> String {
    const UNIQUE: usize = 2_048;
    let mut value = String::from("{");
    for pass in 0..2 {
        for index in (0..UNIQUE).rev() {
            if pass != 0 || index != UNIQUE - 1 {
                value.push(',');
            }
            let _ = write!(value, "\"key-{index:04}\":{}", index + pass * UNIQUE);
        }
    }
    value.push('}');
    value
}

// --- boundary accepts ---------------------------------------------------

fn boundary_accepts() -> Vec<Case> {
    let mut cases = vec![
        deep_array_nesting(),
        deep_object_nesting(),
        long_string_every_escape_form(),
        case(
            "boundary",
            "boundary/surrogate-pair",
            "\"𝄞\"".as_bytes().to_vec(),
            Expect::Accept,
        ),
        raw_unicode_space_string(),
        case("boundary", "boundary/empty-object", b"{}".to_vec(), Expect::Accept),
        case("boundary", "boundary/empty-array", b"[]".to_vec(), Expect::Accept),
        deeply_mixed_containers(),
        case(
            "boundary",
            "boundary/duplicate-keys-last-wins",
            br#"{"a":1,"a":2}"#.to_vec(),
            Expect::Accept,
        ),
    ];
    cases.extend(number_edge_forms());
    cases
}

fn deep_array_nesting() -> Case {
    let mut bytes = "[".repeat(BOUNDARY_NESTING_DEPTH).into_bytes();
    bytes.extend_from_slice(b"0");
    bytes.extend(std::iter::repeat_n(b']', BOUNDARY_NESTING_DEPTH));
    case("boundary", "boundary/deep-array-nesting", bytes, Expect::Accept)
}

fn deep_object_nesting() -> Case {
    let mut bytes = Vec::new();
    for _ in 0..BOUNDARY_NESTING_DEPTH {
        bytes.extend_from_slice(br#"{"k":"#);
    }
    bytes.extend_from_slice(b"0");
    bytes.extend(std::iter::repeat_n(b'}', BOUNDARY_NESTING_DEPTH));
    case("boundary", "boundary/deep-object-nesting", bytes, Expect::Accept)
}

fn long_string_every_escape_form() -> Case {
    // One cycle exercises every JSON escape form once: quote, backslash,
    // solidus, backspace, form-feed, newline, carriage return, tab, a plain
    // BMP `\u` escape, and a surrogate-pair `\u` escape.
    let cycle = r#"\"\\\/\b\f\n\r\tA\uD834\uDD1E"#;
    let mut body = String::new();
    for _ in 0..64 {
        body.push_str(cycle);
    }
    let mut bytes = Vec::with_capacity(body.len() + 2);
    bytes.push(b'"');
    bytes.extend_from_slice(body.as_bytes());
    bytes.push(b'"');
    case("boundary", "boundary/long-string-every-escape", bytes, Expect::Accept)
}

fn raw_unicode_space_string() -> Case {
    // Raw (unescaped) Unicode space and line-separator characters are legal
    // JSON string content: RFC 8259 only requires U+0000-U+001F to be
    // escaped. This guards against a decoder mistakenly treating these as
    // insignificant whitespace or rejecting them as control characters.
    let raw = "\u{00A0}\u{2002}\u{2003}\u{2028}\u{2029}\u{3000}";
    let mut bytes = Vec::new();
    bytes.push(b'"');
    bytes.extend_from_slice(raw.as_bytes());
    bytes.push(b'"');
    case(
        "boundary",
        "boundary/raw-unicode-space-in-string",
        bytes,
        Expect::Accept,
    )
}

fn deeply_mixed_containers() -> Case {
    let bytes = br#"{"a":[1,{"b":[true,false,null,{"c":[[1,2],[3,{"d":"e"}]]}]}],"f":[{"g":1},{"h":[2,3]}]}"#.to_vec();
    case("boundary", "boundary/deeply-mixed-containers", bytes, Expect::Accept)
}

fn number_edge_forms() -> Vec<Case> {
    // Each spelling is chosen to round-trip exactly through `serde_json`'s f64
    // storage (or, for the two extremes, through i64/u64 storage) so a
    // checksum mismatch here means a real divergence rather than the
    // documented arbitrary-precision-vs-f64 floor.
    [
        "-0",
        "0e0",
        "1e-323",
        "1e308",
        "18446744073709551615",
        "-9223372036854775808",
        "0.123456789012345",
        "1E10",
        "1e+10",
        "1E+5",
        "1e-5",
        "2.5e3",
    ]
    .into_iter()
    .map(|spelling| {
        case(
            "boundary",
            format!("boundary/number/{spelling}"),
            spelling.as_bytes().to_vec(),
            Expect::Accept,
        )
    })
    .collect()
}

// --- rejects -------------------------------------------------------------

fn rejects() -> Vec<Case> {
    let mut cases = Vec::new();
    // Non-finite spellings (`nan`, `Infinity`, ...) are excluded from this
    // serde differential: jqf accepts them (compat corpus); serde_json
    // rejects them. This lane is serde parity.
    for spelling in ["01", "-01", "00", "01.5"] {
        cases.push(case(
            "reject",
            format!("reject/leading-zero/{spelling}"),
            spelling.as_bytes().to_vec(),
            Expect::Reject,
        ));
    }
    for (name, bytes) in [
        ("trailing-garbage/true-false", b"true false".to_vec()),
        ("trailing-garbage/two-numbers", b"1 2".to_vec()),
        ("trailing-garbage/two-objects", b"{} {}".to_vec()),
        ("trailing-garbage/trailing-brace", b"[1,2]}".to_vec()),
    ] {
        cases.push(case("reject", format!("reject/{name}"), bytes, Expect::Reject));
    }
    cases.extend(truncated_prefixes());
    // Lone-LOW-surrogate spellings are not here: jqf accepts them (U+FFFD);
    // serde_json rejects them. This lane is serde parity.
    for (name, escape) in [
        ("lone-high-surrogate", r#""\uD800""#),
        ("high-high-surrogate", r#""\uD800\uD800""#),
        ("low-then-high-surrogate", r#""\uDC00\uD800""#),
    ] {
        cases.push(case(
            "reject",
            format!("reject/surrogate/{name}"),
            escape.as_bytes().to_vec(),
            Expect::Reject,
        ));
    }
    cases.extend(invalid_utf8_cases());
    for (name, bytes) in [
        ("nul", vec![b'"', 0x00, b'"']),
        ("unit-separator", vec![b'"', 0x1F, b'"']),
    ] {
        cases.push(case(
            "reject",
            format!("reject/bare-control-char/{name}"),
            bytes,
            Expect::Reject,
        ));
    }
    for (name, bytes) in [
        ("single-quotes", b"'abc'".to_vec()),
        ("unquoted-key", b"{a:1}".to_vec()),
        ("trailing-comma-array", b"[1,]".to_vec()),
        ("trailing-comma-object", br#"{"a":1,}"#.to_vec()),
        ("leading-plus", b"+1".to_vec()),
        ("missing-integer-part", b".5".to_vec()),
        ("missing-fraction-digits", b"1.".to_vec()),
        ("hex-number", b"0x1F".to_vec()),
        ("leading-bom", vec![0xEF, 0xBB, 0xBF, b'1']),
    ] {
        cases.push(case("reject", format!("reject/{name}"), bytes, Expect::Reject));
    }
    cases
}

fn truncated_prefixes() -> Vec<Case> {
    let fixture = br#"{"a":[1,2,"x\n"],"b":true}"#;
    (1..fixture.len())
        .map(|length| {
            case(
                "reject",
                format!("reject/truncated/{length:03}"),
                fixture[..length].to_vec(),
                Expect::Reject,
            )
        })
        .collect()
}

fn invalid_utf8_cases() -> Vec<Case> {
    vec![
        case(
            "reject",
            "reject/invalid-utf8/bare-0xff-in-string",
            vec![b'"', 0xFF, b'"'],
            Expect::Reject,
        ),
        case(
            "reject",
            "reject/invalid-utf8/overlong-encoding",
            vec![b'"', 0xC0, 0x80, b'"'],
            Expect::Reject,
        ),
        case(
            "reject",
            "reject/invalid-utf8/truncated-continuation",
            vec![b'"', 0xE2, 0x82, b'"'],
            Expect::Reject,
        ),
        case(
            "reject",
            "reject/invalid-utf8/between-tokens-after-comma",
            vec![b'[', b'1', b',', 0xFF, b'2', b']'],
            Expect::Reject,
        ),
        case(
            "reject",
            "reject/invalid-utf8/replaces-a-token",
            vec![b'[', 0xFF, b']'],
            Expect::Reject,
        ),
    ]
}
