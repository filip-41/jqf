//! The CSV differential corpus: the record/field product cases from the
//! codec's own fixtures (framing, quoting, embedded CRLF, the unterminated
//! final record, empty input) plus the lineage matrix's policy classes, each
//! paired with the verdict both jqf and the `csv` crate must agree on.
//!
//! A case whose two engines disagree is a DIVERGENCE and fails the run —
//! unless it is one of the declared policy splits listed in `main.rs`'s
//! DECLARED table (the lineage's `lf-policy`, `bare-cr-policy` and
//! `invalid-utf8` classes plus the strict single-document laws). A
//! disagreement that is NOT on the table is a defect.

/// What both decoders are expected to agree on for one corpus case.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Expect {
    /// Both decoders must accept, with equal record/field products.
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

/// Builds the complete CSV corpus.
pub(crate) fn build() -> Vec<Case> {
    let mut cases = Vec::new();
    cases.extend(fixtures());
    cases.extend(quoting_cases());
    cases.extend(rejects());
    cases.extend(declared_splits());
    cases
}

// --- fixtures (the shared RFC 4180 grammar) ---------------------------------

fn fixtures() -> Vec<Case> {
    vec![
        case("fixture", "fixture/simple", b"a,b\nc,d\n".to_vec(), Expect::Accept),
        case("fixture", "fixture/one-record", b"a,b,c\n".to_vec(), Expect::Accept),
        case(
            "fixture",
            "fixture/header-looks-like-data",
            b"name,age\njqf,3\n".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/trailing-empty-field",
            b"a,\n".to_vec(),
            Expect::Accept,
        ),
        case("fixture", "fixture/empty-fields", b",,\n".to_vec(), Expect::Accept),
        case("fixture", "fixture/empty-input", b"".to_vec(), Expect::Accept),
        case(
            "fixture",
            "fixture/multiline-field",
            b"a,\"b\nc\"\nd,e\n".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/crlf-terminators",
            b"a,b\r\nc,d\r\n".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/mixed-terminators",
            b"a,b\nc,d\r\n".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/utf8-content",
            "héllo,wörld\n雪,☃\n".as_bytes().to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/terminated-final-record",
            b"a\nb\n".to_vec(),
            Expect::Accept,
        ),
        // RFC 4180 §2.2: the last record may omit its line break. These two
        // were DECLARED SPLITS while the strict profile faulted the missing
        // terminator; both decoders accept them now (the record carries the
        // missing-terminator advisory), so they are agreement rows.
        case(
            "fixture",
            "fixture/unterminated-final-record",
            b"a,b".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/unterminated-final-record-after-lf",
            b"a\nb".to_vec(),
            Expect::Accept,
        ),
    ]
}

// --- quoting cases -----------------------------------------------------------

fn quoting_cases() -> Vec<Case> {
    vec![
        case(
            "quote",
            "quote/doubled-quote",
            b"\"he said \"\"hi\"\"\",x\n".to_vec(),
            Expect::Accept,
        ),
        case(
            "quote",
            "quote/comma-in-quotes",
            b"\"a,b\",c\n".to_vec(),
            Expect::Accept,
        ),
        case(
            "quote",
            "quote/empty-quoted-field",
            b"\"\",x\n".to_vec(),
            Expect::Accept,
        ),
        case(
            "quote",
            "quote/quoted-multiline",
            b"\"line1\nline2\",x\n".to_vec(),
            Expect::Accept,
        ),
        case("quote", "quote/quoted-crlf", b"\"a\r\nb\",x\n".to_vec(), Expect::Accept),
        case("quote", "quote/quoted-only", b"\"x\"\n".to_vec(), Expect::Accept),
    ]
}

// --- rejects (both decoders must reject) -------------------------------------

fn rejects() -> Vec<Case> {
    vec![
        case(
            "reject",
            "reject/invalid-utf8-bare",
            vec![b'a', 0xFF, b'\n'],
            Expect::Reject,
        ),
        case(
            "reject",
            "reject/invalid-utf8-in-quotes",
            vec![b'"', 0xC3, 0x28, b'"', b'\n'],
            Expect::Reject,
        ),
    ]
}

// --- declared splits (the divergence register's CSV rows) ---------------------
//
// Each case is EXPECTED to disagree, and the disagreement is the point of the
// row: it proves the register against a real incumbent. The reason is written
// here and reprinted by main.rs when the row fires. A row whose case STOPPED
// disagreeing fails the run (the stale-entry rule).

fn declared_splits() -> Vec<Case> {
    vec![
        case(
            "declared",
            "declared/bare-cr-framing-fault",
            // A bare CR not followed by LF: a framing fault for jqf; the csv
            // crate treats a lone CR as a terminator.
            b"a\rb\n".to_vec(),
            Expect::Reject,
        ),
        case("declared", "declared/bare-cr-only", b"\r".to_vec(), Expect::Reject),
        case(
            "declared",
            "declared/initial-byte-order-mark",
            // A source-start BOM is a framing fault for jqf; the csv crate
            // skips it.
            vec![0xEF, 0xBB, 0xBF, b'a', b'\n'],
            Expect::Reject,
        ),
        case(
            "declared",
            "declared/mixed-lengths",
            b"a,b\nc\n,,,\n".to_vec(),
            Expect::Accept,
        ),
        case(
            "declared",
            "declared/blank-line-between",
            // jqf publishes a blank line as a zero-field record (`[]`); the
            // csv crate omits the empty line, so the record products differ.
            b"a,b\n\nc,d\n".to_vec(),
            Expect::Accept,
        ),
        case(
            "declared",
            "declared/mixed-lengths-wider",
            // jqf's rfc4180 dialect has no width law (every record is its own
            // array, header or not); the csv crate's default enforces uniform
            // record width and rejects a WIDER row.
            b"a,b\nc,d,e\n".to_vec(),
            Expect::Accept,
        ),
        case(
            "declared",
            "declared/quote-in-unquoted-field",
            b"ab\"cd,e\n".to_vec(),
            Expect::Reject,
        ),
        case(
            "declared",
            "declared/quote-mid-field",
            // RFC 4180 forbids the input (a quote inside an unquoted field).
            // jqf now REJECTS it as a malformed field (InvalidInput — batch-6
            // B8: a quote opens quoted state only at a field start); the csv
            // crate treats the quote as literal data.
            b"a\"b\",c\n".to_vec(),
            Expect::Reject,
        ),
        case(
            "declared",
            "declared/unclosed-quote",
            // An unterminated quote: jqf requires closure (the record fails);
            // the csv crate extends the quoted field to end of input.
            b"\"a,b\n".to_vec(),
            Expect::Reject,
        ),
        case(
            "declared",
            "declared/quote-then-garbage",
            b"a,\"b\"\"\n".to_vec(),
            Expect::Reject,
        ),
        case(
            "declared",
            "declared/quote-not-closed-at-eof",
            b"a,\"b".to_vec(),
            Expect::Reject,
        ),
    ]
}
