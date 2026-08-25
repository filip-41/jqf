//! The CBOR differential corpus: the codec's own smoke fixtures (the decode
//! corpus and the reject corpus) plus boundary accepts and rejects, each
//! paired with the verdict both jqf and `ciborium` must agree on.
//!
//! A case whose two engines disagree is a DIVERGENCE and fails the run —
//! unless it is one of the declared policy splits listed in `main.rs`'s
//! DECLARED table. Those rows are written up-front from the product laws
//! (§5.6.1 tag projection, fail-closed duplicate/non-text keys); a
//! disagreement that is NOT on the table is a defect.

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
}

fn case(category: &'static str, name: impl Into<String>, bytes: Vec<u8>, expect: Expect) -> Case {
    Case {
        category,
        name: name.into(),
        bytes,
        expect,
    }
}

/// Builds the complete CBOR corpus.
pub(crate) fn build() -> Vec<Case> {
    let mut cases = Vec::new();
    cases.extend(fixtures());
    cases.extend(boundary_accepts());
    cases.extend(rejects());
    cases.extend(declared_splits());
    cases
}

// --- fixtures (seeded from the codec's own smoke battery) -------------------

#[expect(
    clippy::too_many_lines,
    reason = "one fixture table per codec, seeded from the codec's own smoke battery"
)]
fn fixtures() -> Vec<Case> {
    vec![
        // Scalars, integer encodings, floats in every width.
        case("fixture", "fixture/uint-0", vec![0x00], Expect::Accept),
        case("fixture", "fixture/uint-1", vec![0x01], Expect::Accept),
        case("fixture", "fixture/uint-23", vec![0x17], Expect::Accept),
        case("fixture", "fixture/uint-24", vec![0x18, 0x18], Expect::Accept),
        case("fixture", "fixture/uint-256", vec![0x19, 0x01, 0x00], Expect::Accept),
        case(
            "fixture",
            "fixture/uint-65536",
            vec![0x1a, 0x00, 0x01, 0x00, 0x00],
            Expect::Accept,
        ),
        case("fixture", "fixture/negint-1", vec![0x20], Expect::Accept),
        case(
            "fixture",
            "fixture/negint-i64-min",
            vec![0x3b, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            Expect::Accept,
        ),
        case("fixture", "fixture/bool-false", vec![0xf4], Expect::Accept),
        case("fixture", "fixture/bool-true", vec![0xf5], Expect::Accept),
        case("fixture", "fixture/null", vec![0xf6], Expect::Accept),
        case(
            "fixture",
            "fixture/float-half-1.0",
            vec![0xf9, 0x3c, 0x00],
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/float-half-1.5",
            vec![0xf9, 0x3e, 0x00],
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/float-single-1.0",
            vec![0xfa, 0x3f, 0x80, 0x00, 0x00],
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/float-double-1.5",
            vec![0xfb, 0x3f, 0xf8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/neg-float",
            vec![0xfb, 0xbf, 0xf0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            Expect::Accept,
        ),
        case("fixture", "fixture/float-zero", vec![0xf9, 0x00, 0x00], Expect::Accept),
        case(
            "fixture",
            "fixture/float-neg-zero",
            vec![0xf9, 0x80, 0x00],
            Expect::Accept,
        ),
        // Text and byte strings, definite and indefinite.
        case("fixture", "fixture/text-a", vec![0x61, 0x61], Expect::Accept),
        case(
            "fixture",
            "fixture/text-hello",
            vec![0x65, 0x68, 0x65, 0x6c, 0x6c, 0x6f],
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/text-unicode",
            vec![0x63, 0xe2, 0x98, 0x9e],
            Expect::Accept,
        ),
        case("fixture", "fixture/bytes-empty", vec![0x40], Expect::Accept),
        case(
            "fixture",
            "fixture/bytes-1-2-3-4",
            vec![0x44, 0x01, 0x02, 0x03, 0x04],
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/text-indefinite",
            vec![0x7f, 0x63, 0x66, 0x6f, 0x6f, 0x63, 0x62, 0x61, 0x72, 0xff],
            Expect::Accept,
        ),
        // Arrays, maps, indefinite containers.
        case("fixture", "fixture/array-empty", vec![0x80], Expect::Accept),
        case(
            "fixture",
            "fixture/array-1-2-3",
            vec![0x83, 0x01, 0x02, 0x03],
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/array-indefinite",
            vec![0x9f, 0x01, 0x02, 0xff],
            Expect::Accept,
        ),
        case("fixture", "fixture/map-empty", vec![0xa0], Expect::Accept),
        case(
            "fixture",
            "fixture/map-a-1-b-true-null",
            vec![0xa2, 0x61, 0x61, 0x01, 0x61, 0x62, 0x82, 0xf5, 0xf6],
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/map-indefinite",
            vec![0xbf, 0x61, 0x61, 0x01, 0xff],
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/nested-mixed",
            vec![
                0xa2, 0x61, 0x6b, 0x82, 0x01, 0x82, 0x02, 0x03, 0x61, 0x6d, 0xa1, 0x61, 0x78, 0xf5,
            ],
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/map-order-difference",
            vec![0xa2, 0x61, 0x62, 0x02, 0x61, 0x61, 0x01],
            Expect::Accept,
        ),
    ]
}

// --- boundary accepts ------------------------------------------------------

fn boundary_accepts() -> Vec<Case> {
    vec![
        case(
            "boundary",
            "boundary/uint-u64-max",
            vec![0x1b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            Expect::Accept,
        ),
        case(
            "boundary",
            "boundary/bignum-positive",
            vec![0xc2, 0x44, 0x01, 0x00, 0x00, 0x00],
            Expect::Accept,
        ),
        case(
            "boundary",
            "boundary/bignum-negative",
            vec![0xc3, 0x41, 0x01],
            Expect::Accept,
        ),
        case(
            "boundary",
            "boundary/tag-unknown-simple",
            vec![0xd9, 0xd9, 0xf7, 0x61, 0x78],
            Expect::Accept,
        ),
        case(
            "boundary",
            "boundary/tag-chain",
            vec![0xd9, 0xd9, 0xf7, 0xd8, 0x22, 0x82, 0x01, 0x02],
            Expect::Accept,
        ),
    ]
}

// --- rejects (both decoders must reject) ------------------------------------

fn rejects() -> Vec<Case> {
    vec![
        case(
            "reject",
            "reject/trailing-bytes",
            vec![0x81, 0x01, 0x00],
            Expect::Reject,
        ),
        case("reject", "reject/reserved-ai-28", vec![0x1c], Expect::Reject),
        case("reject", "reject/reserved-simple-31", vec![0xf8, 0x1f], Expect::Reject),
        case(
            "reject",
            "reject/simple-other-19",
            // Two-byte simple 19 (`0xf8 0x13`): RFC 8949 §3.3 reserves the
            // two-byte form for values below 32. Both sides reject.
            vec![0xf8, 0x13],
            Expect::Reject,
        ),
        case("reject", "reject/invalid-utf8-text", vec![0x61, 0xff], Expect::Reject),
        case("reject", "reject/truncated-uint", vec![0x19, 0x01], Expect::Reject),
        case("reject", "reject/truncated-text", vec![0x63, 0x61], Expect::Reject),
        case("reject", "reject/truncated-array", vec![0x82, 0x01], Expect::Reject),
        case(
            "reject",
            "reject/unterminated-indefinite",
            vec![0x9f, 0x01, 0x02],
            Expect::Reject,
        ),
        case("reject", "reject/break-outside-indefinite", vec![0xff], Expect::Reject),
        case(
            "reject",
            "reject/empty-string-then-data",
            vec![0x60, 0x00],
            Expect::Reject,
        ),
    ]
}

// --- declared splits (the divergence register's CBOR rows) ------------------
//
// Each case is EXPECTED to disagree, and the disagreement is the point of the
// row: it proves the register against a real incumbent. The reason is written
// here and reprinted by main.rs when the row fires. A row whose case STOPPED
// disagreeing fails the run (the stale-entry rule).

fn declared_splits() -> Vec<Case> {
    vec![
        case(
            "declared",
            "declared/duplicate-key",
            // §5.6.1: jqf fails closed on duplicate map keys; ciborium's
            // ordered Vec keeps both entries silently.
            vec![0xa2, 0x61, 0x61, 0x01, 0x61, 0x61, 0x02],
            Expect::Reject,
        ),
        case(
            "declared",
            "declared/non-text-key",
            // jqf's object keys are core strings; a non-text CBOR map key is
            // Unrepresentable. ciborium keeps any value as a key.
            vec![0xa1, 0x01, 0x61, 0x61],
            Expect::Reject,
        ),
        case(
            "declared",
            "declared/undefined-simple",
            // Simple 23 (`undefined`): jqf preserves the fact as
            // `cbor:simple:23`; ciborium folds it into `null`.
            vec![0xf7],
            Expect::Accept,
        ),
        case(
            "declared",
            "declared/tag0-datetime",
            // Tag 0 (RFC 3339 datetime): jqf projects to an OffsetDateTime;
            // ciborium retains every tag as a Tag.
            vec![
                0xc0, 0x74, 0x32, 0x30, 0x31, 0x33, 0x2d, 0x30, 0x33, 0x2d, 0x32, 0x31, 0x54, 0x32, 0x30, 0x3a, 0x30,
                0x34, 0x3a, 0x30, 0x30, 0x5a,
            ],
            Expect::Accept,
        ),
        case(
            "declared",
            "declared/tag1-epoch-int",
            // Tag 1 (epoch seconds): jqf projects to an OffsetDateTime;
            // ciborium retains the tag.
            vec![0xc1, 0x00],
            Expect::Accept,
        ),
        case(
            "declared",
            "declared/tag1-epoch-float",
            vec![0xc1, 0xfb, 0x3f, 0xf8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            Expect::Accept,
        ),
        case(
            "declared",
            "declared/tag4-decimal-fraction",
            // Tag 4 (decimal fraction): jqf projects to an exact Decimal; the
            // incumbent retains the tag (and cannot represent the exactness).
            vec![0xc4, 0x82, 0x21, 0x19, 0x01, 0x00],
            Expect::Accept,
        ),
        case(
            "declared",
            "declared/tag5-bigfloat",
            vec![0xc5, 0x82, 0x20, 0x03],
            Expect::Accept,
        ),
    ]
}
