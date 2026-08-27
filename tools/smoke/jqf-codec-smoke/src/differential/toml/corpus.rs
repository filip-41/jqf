//! The TOML differential corpus: real fixtures from the codec's own tests
//! (the roundtrip battery and the smoke's Cargo.toml-shaped document) plus
//! boundary accepts and rejects, each paired with the verdict both jqf and
//! the `toml` crate must agree on.
//!
//! A case whose two engines disagree is a DIVERGENCE and fails the run —
//! unless it is one of the declared policy splits listed in `main.rs`'s
//! DECLARED table. Those rows are written up-front from the product laws
//! (exact arithmetic, fail-closed duplicate keys, the legacy `inf`/`nan`
//! spellings); a disagreement that is NOT on the table is a defect.

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

/// Builds the complete TOML corpus.
pub(crate) fn build() -> Vec<Case> {
    let mut cases = Vec::new();
    cases.extend(fixtures());
    cases.extend(number_forms());
    cases.extend(datetime_forms());
    cases.extend(boundary_accepts());
    cases.extend(rejects());
    cases.extend(declared_splits());
    cases
}

// --- fixtures (seeded from the codec's own tests) ------------------------

fn fixtures() -> Vec<Case> {
    vec![
        cargo_toml_shaped(),
        case(
            "fixture",
            "fixture/basic-scalars",
            b"title = \"TOML Example\"\ncount = 42\nratio = 3.14\nok = true\nnothing = \"\"\n"
                .to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/dotted-and-table",
            b"a.b = 1\n[server]\nhost = \"example.org\"\nport = 8080\n".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/array-of-tables",
            b"[[product]]\nname = \"Hammer\"\n[[product]]\nname = \"Nail\"\n".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/radix-underscore-signed",
            b"hex = 0x1F\nhex2 = 0x10\noct = 0o17\nbin = 0b101\nbig = 1_000_000\nneg = -17\nfloat = 1.5e2\n"
                .to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/radix-underscores",
            b"a = 0xff_ff\nb = 0xFF_FF\nc = 0o7_55\nd = 0b1101_0110\n".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/inline-tables",
            b"point = { x = 1, y = 2 }\nname = { first = \"Tom\", last = \"P\" }\n".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/inline-nested-dotted",
            b"animal = { type.name = \"pug\" }\n".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/string-escapes",
            b"title = \"TOML\"\nlit = 'C:\\\\Users\\\\x'\ncount = 42\nneg = -7\nradix = 0x2A\nunderscored = 1_000\nsigned = +5\nescaped = \"a\\nb\"\n"
                .to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/comments",
            b"a = 1 # c\nb = 2\n".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/nested-table-after-aot",
            b"[[a]]\nx = 1\n[a.b]\ny = 2\n".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/multiline-strings",
            b"a = \"\"\"\nline1\nline2\n\"\"\"\nb = '''\nraw\n'''\n".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/unicode-quoted-keys",
            "\"k\\u0065y\" = \"\\u0062\"\n\"mu\\u006cti\" = \"\"\"\nline\n\"\"\"\n"
                .as_bytes()
                .to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/empty-input",
            b"".to_vec(),
            Expect::Accept,
        ),
    ]
}

fn cargo_toml_shaped() -> Case {
    // The smoke battery's realistic document: mixed statement kinds, each with
    // a trailing comment — headers, arrays-of-tables, inline tables and every
    // scalar kind in one document.
    let text = "\
[package] # crate metadata
name = \"jqf\" # crate name
version = \"0.0.0\" # semver
edition = \"2024\" # rust edition
publish = false # never publish

[dependencies] # runtime deps
jqf-data = { path = \"../jqf-data\" } # workspace path dep
serde = \"1\" # crates.io dep

[[bin]] # one binary target
name = \"jqf\" # binary name
path = \"src/main.rs\" # entry point

[features] # cargo features
default = [\"std\"] # default feature set
";
    case(
        "fixture",
        "fixture/cargo-toml-shaped",
        text.as_bytes().to_vec(),
        Expect::Accept,
    )
}

// --- number forms ---------------------------------------------------------

fn number_forms() -> Vec<Case> {
    [
        "a = 5.0\n",
        "a = 1e5\n",
        "a = 1e15\n",
        "a = 2.5e3\n",
        "a = -0.0\n",
        "a = +99\n",
        "a = 0\n",
        "a = -0\n",
        "a = 1234567890123456789\n",
        "a = 3.141592653589793\n",
        "a = 1.0e-3\n",
        "a = 5e+22\n",
        "a = 6.626e-34\n",
        "a = [1, 2, 3,]\n",
        // The legacy `inf`/`nan` spellings: jqf keeps them as binary64 and
        // the incumbent `toml` crate (spec-1.1 line) accepts them too, so
        // these are agreement cases, not declared splits.
        "a = inf\n",
        "a = -inf\n",
        "a = nan\n",
    ]
    .into_iter()
    .map(|text| {
        case(
            "number",
            format!("number/{text:?}"),
            text.as_bytes().to_vec(),
            Expect::Accept,
        )
    })
    .collect()
}

// --- datetime forms --------------------------------------------------------

fn datetime_forms() -> Vec<Case> {
    [
        "d1 = 1979-05-27\nt1 = 07:32:00\nodt = 1979-05-27T07:32:00Z\nldt = 1979-05-27T00:32:00.999999\n",
        "a = 1979-05-27T07:32:00\n",
        "a = 1979-05-27 07:32:00\n",
        "a = 1979-05-27T07:32:00+07:00\n",
        "a = 1979-05-27T07:32:00.5\n",
        "a = 1979-05-27T07:32:00.50\n",
        "a = 07:32:00.999999\n",
        "a = 1979-05-27T00:00:00.000000001\n",
        "a = 1979-05-27T07:32:60\n",
        // ABNF `"Z"` is case-insensitive: lowercase `z` is UTC. Retired from
        // the declared-split table when the decoder started accepting it.
        "a = 1979-05-27T07:32:00z\n",
    ]
    .into_iter()
    .map(|text| {
        case(
            "datetime",
            format!("datetime/{text:?}"),
            text.as_bytes().to_vec(),
            Expect::Accept,
        )
    })
    .collect()
}

// --- boundary accepts ------------------------------------------------------

fn boundary_accepts() -> Vec<Case> {
    vec![
        case(
            "boundary",
            "boundary/deep-ish-nesting",
            b"a = [[[[1]]]]\nb = { x = { y = { z = 1 } } }\n".to_vec(),
            Expect::Accept,
        ),
        case(
            "boundary",
            "boundary/array-of-tables-of-inline",
            b"[[s]]\nv = { a = 1, b = [true, false] }\n[[s]]\nv = { c = \"x\" }\n".to_vec(),
            Expect::Accept,
        ),
    ]
}

// --- rejects (both decoders must reject) -----------------------------------

fn rejects() -> Vec<Case> {
    let mut cases = Vec::new();
    let table: [(&str, Vec<u8>); 27] = [
        ("leading-zero", b"a = 01\n".to_vec()),
        ("float-dangling-point", b"a = 1.\n".to_vec()),
        ("float-leading-point", b"a = .5\n".to_vec()),
        ("float-exponent-dangling-point", b"a = 1.e5\n".to_vec()),
        ("float-plus-leading-point", b"a = +.5\n".to_vec()),
        ("radix-leading-underscore", b"a = 0x_ff\n".to_vec()),
        ("radix-trailing-underscore", b"a = 0xff_\n".to_vec()),
        ("radix-double-underscore", b"a = 0xf__f\n".to_vec()),
        ("trailing-garbage", b"a = 1\nb = 2 garbage".to_vec()),
        ("bare-word-value", b"a = hello\n".to_vec()),
        ("truee", b"a = truee\n".to_vec()),
        ("bare-cr", b"a = 1 # c\rmore\n".to_vec()),
        ("garbage-after-value", b"a = 1 garbage\n".to_vec()),
        ("unclosed-string", b"a = \"unclosed\n".to_vec()),
        ("unquoted-value", b"a = hello\n".to_vec()),
        ("double-underscore-number", b"a = 1__0\n".to_vec()),
        ("leading-zero-underscore", b"a = 0_1\n".to_vec()),
        ("two-values-one-line", b"a = 1 b = 2\n".to_vec()),
        ("invalid-month", b"a = 1979-13-01\n".to_vec()),
        ("invalid-day", b"a = 1979-02-30\n".to_vec()),
        ("invalid-hour", b"a = 1979-05-27T25:00:00\n".to_vec()),
        (
            "control-char-in-string",
            [b"a = \"".to_vec(), vec![0x01], b"\"\n".to_vec()].concat(),
        ),
        (
            "invalid-utf8",
            [b"a = \"".to_vec(), vec![0xFF], b"\"\n".to_vec()].concat(),
        ),
        // Fail-closed duplicate-key law: both jqf and the incumbent reject
        // redefinition, so these are agreement cases.
        ("duplicate-key", b"a = 1\na = 2\n".to_vec()),
        ("duplicate-table", b"[a]\nx = 1\n[a]\ny = 2\n".to_vec()),
        ("table-after-aot", b"[a]\nx = 1\n[[a]]\ny = 2\n".to_vec()),
        // The incumbent's `Value::Integer` is i64 and jqf's TOML grammar
        // agrees: an integer past i64 range is rejected on both sides.
        ("int-overflow", b"a = 9223372036854775808\n".to_vec()),
    ];
    for (name, bytes) in table {
        cases.push(case("reject", format!("reject/{name}"), bytes, Expect::Reject));
    }
    cases
}

// --- declared splits (the divergence register's TOML rows) ------------------
//
// Each case is EXPECTED to disagree, and the disagreement is the point of the
// row: it proves the register against a real incumbent. The reason is written
// here and reprinted by main.rs when the row fires. A row whose case STOPPED
// disagreeing fails the run (the stale-entry rule).

fn declared_splits() -> Vec<Case> {
    vec![
        case(
            "declared",
            "declared/exact-decimal-split",
            b"a = 0.123456789012345678901\n".to_vec(),
            // jqf retains the exact decimal spelling; `toml` rounds to f64.
            Expect::Accept,
        ),
        case(
            "declared",
            "declared/huge-exponent-split",
            b"a = 1e400\n".to_vec(),
            // jqf's exact arithmetic accepts 1e400; `toml`'s f64 storage
            // errors out of range.
            Expect::Accept,
        ),
        case(
            "declared",
            "declared/negative-zero-offset-split",
            b"a = 1979-05-27T07:32:00-00:00\n".to_vec(),
            // jqf keeps the unknown-local-offset fact (`-00:00`); the `toml`
            // crate normalizes it to a zero offset and cannot re-render it.
            Expect::Accept,
        ),
    ]
}
