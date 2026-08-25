//! End-to-end TOML decode/encode tests: parse a source into a `Document`, materialize it, and round-trip through the
//! deterministic encoder.

mod common;

use jqf_codec_core::{
    AccessGuarantees, AccessOutcome, AccessRequirement, CodecDemand, CodecError, CodecFailureKind, CodecRunContext,
    DecodeRequest, DemandClause, DiagnosticPolicy, EncodeItem, EncodeRequest, ValidationMode,
};

use jqf_data::{DialectId, FormatId, Value};
use jqf_resource::ResourceContext;
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

fn source(bytes: &[u8]) -> ResolvedSource<'_> {
    ResolvedSource::new(
        SourceRef::new(SourceId::new(92), SourceKind::Input),
        "test.toml",
        bytes,
        0,
    )
}

fn whole_requirement(resources: &ResourceContext<'_>) -> AccessRequirement {
    let mut demand = CodecDemand::try_new(resources);
    demand.try_insert(&DemandClause::SemanticRoot).expect("semantic root");
    demand.try_insert(&DemandClause::ValueShape).expect("value shape");
    AccessRequirement::try_whole(
        demand,
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        resources,
    )
    .expect("requirement")
}

fn decode(bytes: &[u8]) -> Result<Value, CodecError> {
    let mut resources = common::resources();
    let registration = jqf_codec_toml::registration_1_0().expect("registration");
    let format = FormatId::try_new(jqf_codec_toml::FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(jqf_codec_toml::TOML_1_0_DIALECT_ID).expect("dialect");
    let _ = format;
    let _ = dialect;
    let mut provider = registration.decoder().expect("decoder").create_provider(
        source(bytes),
        DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: &DialectId::try_new(jqf_codec_toml::TOML_JQF_1_0_DIALECT_ID).expect("dialect"),
            options: None,
            allow_adjacent_values: false,
            value_separator: &[],
        },
        &mut resources,
    )?;
    let requirement = whole_requirement(&resources);
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources)?;
    let result = {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run)?
    };
    let AccessOutcome::FullDocument(product) = result.outcome() else {
        panic!("expected full document");
    };
    product.document().materialize_root(&mut resources).map_err(|_| {
        CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "materialize TOML root",
        })
    })
}

fn encode(value: &Value, resources: &mut ResourceContext<'_>) -> Result<Vec<u8>, CodecError> {
    let format = FormatId::try_new(jqf_codec_toml::FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(jqf_codec_toml::TOML_JQF_1_0_DIALECT_ID).expect("dialect");
    let registration = jqf_codec_toml::registration_1_0().expect("registration");
    let factory = registration.encoder().expect("encoder").create_factory(
        EncodeRequest {
            format: &format,
            dialect: &dialect,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            preservation: jqf_codec_core::PreservationRequest::None,
            options: None,
        },
        resources,
    )?;
    let mut session = factory
        .start(
            EncodeItem::Owned(value),
            jqf_codec_core::PreservationRequest::None,
            resources,
        )
        .expect("session");
    let mut out = Vec::new();
    {
        let mut sink = jqf_codec_core::VecByteSink::new(&mut out);
        let mut run = CodecRunContext::new(resources);
        run.set_cooperative_credits(4_096);
        session.encode(&mut sink, &mut run)?;
    }
    Ok(out)
}

fn decode_encode_roundtrip(input: &str) -> Result<String, CodecError> {
    let value = decode(input.as_bytes())?;
    let mut resources = common::resources();
    let bytes = encode(&value, &mut resources)?;
    Ok(String::from_utf8(bytes).expect("UTF-8 output"))
}

/// The semantic tree of a simple document materializes correctly.
#[test]
fn simple_scalars_materialize() {
    let value =
        decode(b"title = \"TOML Example\"\ncount = 42\nratio = 3.14\nok = true\nnothing = \"\"\n").expect("decode");
    match value {
        Value::Object(object) => {
            assert_eq!(object.len(), 5);
            assert_eq!(
                object.get("title").and_then(|v| match v {
                    Value::String(s) => Some(s.as_str().to_owned()),
                    _ => None,
                }),
                Some("TOML Example".to_owned())
            );
            assert!(
                matches!(object.get("count"), Some(Value::Number(n)) if n.category() == jqf_data::NumberCategory::Integer)
            );
            // A finite float spelling decodes as an EXACT decimal, not a binary64.
            assert!(matches!(object.get("ratio"), Some(Value::Number(n)) if n.as_decimal().is_some()));
            assert!(matches!(object.get("ok"), Some(Value::Bool(true))));
        }
        other => panic!("expected object root, got {other:?}"),
    }
}

/// Nested tables and dotted keys build the right structure.
#[test]
fn tables_and_dotted_keys_materialize() {
    let value = decode(b"a.b = 1\n[server]\nhost = \"example.org\"\nport = 8080\n").expect("decode");
    match &value {
        Value::Object(root) => {
            // `a.b = 1` creates table `a` containing key `b` with value 1.
            let a = root.get("a").expect("table a");
            let Value::Object(a) = a else { panic!("a is an object") };
            let b = a.get("b").expect("key b in table a");
            assert!(matches!(b, Value::Number(n) if n.category() == jqf_data::NumberCategory::Integer));

            let server = root.get("server").expect("table server");
            let Value::Object(server) = server else {
                panic!("server is an object")
            };
            assert!(matches!(server.get("host"), Some(Value::String(s)) if s.as_str() == "example.org"));
        }
        other => panic!("expected object root, got {other:?}"),
    }
}

/// Arrays-of-tables become arrays of objects.
#[test]
fn array_of_tables_materializes() {
    let value = decode(b"[[product]]\nname = \"Hammer\"\n[[product]]\nname = \"Nail\"\n").expect("decode");
    match &value {
        Value::Object(root) => {
            let product = root.get("product").expect("array-of-tables");
            match product {
                Value::Array(array) => {
                    assert_eq!(array.len(), 2);
                    for element in array {
                        let Value::Object(object) = element else {
                            panic!("element is an object");
                        };
                        assert!(object.get("name").is_some());
                    }
                }
                other => panic!("product is an array, got {other:?}"),
            }
        }
        other => panic!("expected object root, got {other:?}"),
    }
}

/// Numbers: hex, octal, binary, underscores, floats, infinities.
#[test]
fn number_forms_materialize() {
    let value = decode(
        b"hex = 0x1F\nhex2 = 0x10\noct = 0o17\nbin = 0b101\nbig = 1_000_000\nneg = -17\nfloat = 1.5e2\ninf = inf\nninf = -inf\n",
    )
    .expect("decode");
    let Value::Object(object) = &value else {
        panic!("object")
    };
    let integer = |key: &str| -> i64 {
        match object.get(key) {
            Some(Value::Number(n)) => match n.to_integer() {
                Some(i) => i.as_str().parse().expect("int"),
                None => panic!("{key} is not an integer"),
            },
            _ => panic!("{key} missing"),
        }
    };
    assert_eq!(integer("hex"), 31);
    assert_eq!(integer("hex2"), 16);
    assert_eq!(integer("oct"), 15);
    assert_eq!(integer("bin"), 5);
    assert_eq!(integer("big"), 1_000_000);
    assert_eq!(integer("neg"), -17);
    // `1.5e2` decodes as an EXACT decimal; the `.inf` spellings keep the binary64 kind.
    let decimal = |key: &str| -> (String, i64) {
        match object.get(key) {
            Some(Value::Number(n)) => {
                let decimal = n.as_decimal().expect("decimal");
                (decimal.coefficient().as_str().to_owned(), decimal.scale())
            }
            _ => panic!("{key} missing"),
        }
    };
    assert_eq!(decimal("float"), ("15".to_owned(), -1));
    let float = |key: &str| -> f64 {
        match object.get(key) {
            Some(Value::Number(n)) => n.as_float().expect("float").get(),
            _ => panic!("{key} missing"),
        }
    };
    assert!(float("inf").is_infinite() && float("inf") > 0.0);
    assert!(float("ninf").is_infinite() && float("ninf") < 0.0);
}

/// Underscores between radix digits are valid for every radix, not just decimal: a hex digit letter (`a`-`f`/`A`-`F`)
/// is a legal underscore neighbor exactly like a decimal digit is — `0xff_ff` was rejected while
/// `0o7_55`/`0b1101_0110` were already correct.
#[test]
fn underscore_neighbors_a_hex_letter_in_every_radix() {
    let value = decode(b"a = 0xff_ff\nb = 0xFF_FF\nc = 0o7_55\nd = 0b1101_0110\n").expect("decode");
    let Value::Object(object) = &value else {
        panic!("object")
    };
    let integer = |key: &str| -> i64 {
        match object.get(key) {
            Some(Value::Number(n)) => match n.to_integer() {
                Some(i) => i.as_str().parse().expect("int"),
                None => panic!("{key} is not an integer"),
            },
            _ => panic!("{key} missing"),
        }
    };
    assert_eq!(integer("a"), 0xff_ff);
    assert_eq!(integer("b"), 0xFF_FF);
    assert_eq!(integer("c"), 0o7_55);
    assert_eq!(integer("d"), 0b1101_0110);
}

/// The misplaced-underscore rules still hold for a radix integer: a leading, trailing, or doubled underscore is
/// rejected exactly like the decimal case.
#[test]
fn misplaced_underscore_in_a_radix_integer_is_still_rejected() {
    for bytes in [
        b"a = 0x_ff\n".as_slice(),
        b"a = 0xff_\n".as_slice(),
        b"a = 0xf__f\n".as_slice(),
    ] {
        assert!(decode(bytes).is_err(), "{bytes:?} should be rejected");
    }
}

/// Local dates and date-times project to temporal kinds.
#[test]
fn temporals_materialize() {
    let value =
        decode(b"d1 = 1979-05-27\nt1 = 07:32:00\nodt = 1979-05-27T07:32:00Z\nldt = 1979-05-27T00:32:00.999999\n")
            .expect("decode");
    let Value::Object(object) = &value else {
        panic!("object")
    };
    assert!(matches!(object.get("d1"), Some(Value::LocalDate(_))));
    assert!(matches!(object.get("t1"), Some(Value::LocalTime(_))));
    assert!(matches!(object.get("odt"), Some(Value::OffsetDateTime(_))));
    assert!(matches!(object.get("ldt"), Some(Value::LocalDateTime(_))));
}

/// Inline tables become objects.
#[test]
fn inline_tables_materialize() {
    let value = decode(b"point = { x = 1, y = 2 }\nname = { first = \"Tom\", last = \"P\" }\n").expect("decode");
    let Value::Object(object) = &value else {
        panic!("object")
    };
    let point = object.get("point").expect("point");
    let Value::Object(point) = point else {
        panic!("point is an object")
    };
    assert_eq!(point.len(), 2);
}

/// A dotted key inside an inline table — TOML 1.0.0's own example — builds the same implicit nested table a
/// top-level dotted key does.
#[test]
fn dotted_key_inside_inline_table_materializes() {
    let value = decode(b"animal = { type.name = \"pug\" }\n").expect("decode");
    let Value::Object(root) = &value else { panic!("object") };
    let Value::Object(animal) = root.get("animal").expect("animal") else {
        panic!("animal is an object")
    };
    let Value::Object(kind) = animal.get("type").expect("type") else {
        panic!("type is an object")
    };
    let Value::String(name) = kind.get("name").expect("name") else {
        panic!("name is a string")
    };
    assert_eq!(name.as_str(), "pug");
}

/// Two dotted keys sharing a table prefix MERGE into the same implicit table, exactly like two top-level dotted-key
/// statements do.
#[test]
fn dotted_keys_inside_inline_table_merge_a_shared_prefix() {
    let value = decode(b"x = { a.b = 1, a.c = 2 }\n").expect("decode");
    let Value::Object(root) = &value else { panic!("object") };
    let Value::Object(x) = root.get("x").expect("x") else {
        panic!("x is an object")
    };
    let Value::Object(a) = x.get("a").expect("a") else {
        panic!("a is an object")
    };
    assert!(matches!(a.get("b"), Some(Value::Number(n)) if n.category() == jqf_data::NumberCategory::Integer));
    assert!(matches!(a.get("c"), Some(Value::Number(n)) if n.category() == jqf_data::NumberCategory::Integer));
}

/// A deeper dotted chain merges the same way, arbitrarily nested.
#[test]
fn deeply_dotted_keys_inside_inline_table_merge() {
    let value = decode(b"x = { a.b.c = 1, a.b.d = 2 }\n").expect("decode");
    let Value::Object(root) = &value else { panic!("object") };
    let Value::Object(x) = root.get("x").expect("x") else {
        panic!("x is an object")
    };
    let Value::Object(a) = x.get("a").expect("a") else {
        panic!("a is an object")
    };
    let Value::Object(b) = a.get("b").expect("b") else {
        panic!("b is an object")
    };
    assert!(matches!(b.get("c"), Some(Value::Number(n)) if n.category() == jqf_data::NumberCategory::Integer));
    assert!(matches!(b.get("d"), Some(Value::Number(n)) if n.category() == jqf_data::NumberCategory::Integer));
}

/// A dotted key cannot redefine a key already assigned in the same inline table: the same conflict rules as a top-level
/// dotted key.
#[test]
fn dotted_key_conflicts_inside_inline_table_are_rejected() {
    for bytes in [
        b"x = { a.b = 1, a.b = 2 }\n".as_slice(),
        b"x = { a = 1, a.b = 2 }\n".as_slice(),
        b"x = { a.b = 1, a = 2 }\n".as_slice(),
        b"x = { a.b = 1, a.b.c = 2 }\n".as_slice(),
    ] {
        assert!(decode(bytes).is_err(), "{bytes:?} should be rejected");
    }
}

/// The container-span frontier must never defer an implicit (dotted-key created) inline table to a span: it has no
/// literal `{...}` delimiters in source, so no span exists that a lazy toucher could soundly re-parse. The value must
/// materialize identically to the eager decode at every forced frontier depth, and no span may be committed for it.
#[test]
fn forced_frontier_never_defers_an_implicit_dotted_table() {
    let input = b"animal = { type.name = \"pug\" }\n";
    let (eager, eager_spans) = decode_with_frontier(input, None).expect("eager");
    assert_eq!(eager_spans, 0);
    for depth in [0, 1, 2, 3] {
        let (lazy, _lazy_spans) = decode_with_frontier(input, Some(depth)).expect("lazy at depth {depth}");
        assert!(
            values_equal(&lazy, &eager),
            "frontier {depth} diverged from the eager value"
        );
    }
}

/// An unterminated multiline literal holding one 2-byte scalar (`a='''` plus U+018B) is rejected as invalid input,
/// never a panic.
#[test]
fn multiline_string_scalar_advances_past_its_continuation_bytes() {
    // `corpus/crash-toml-multiline-utf8/min-7.bin`.
    let error = decode(b"a='''\xc6\x8b").expect_err("unterminated multiline string");
    assert!(matches!(error.kind(), CodecFailureKind::InvalidInput));

    // Trailing content after a 2-byte scalar must decode or reject, never panic. `artifact-35.bin`:
    let artifact_35: &[u8] = &[
        0x61, 0x3d, 0x27, 0x27, 0x27, 0x3f, 0x3f, 0x31, 0x31, 0x3f, 0x3f, 0x3f, 0x61, 0x20, 0x3d, 0x30, 0x33, 0x33,
        0x33, 0x33, 0x23, 0x33, 0x33, 0x3f, 0x3f, 0x3f, 0x3f, 0x3f, 0x3f, 0x3f, 0x33, 0x37, 0x3a, 0xc6, 0x8b,
    ];
    // `artifact-50.bin`:
    let artifact_50: &[u8] = &[
        0x61, 0x3d, 0x27, 0x27, 0x27, 0x3f, 0x3f, 0x31, 0x31, 0x3f, 0x3f, 0x3f, 0x61, 0x20, 0x3d, 0x20, 0x33, 0x33,
        0x33, 0x33, 0x33, 0x33, 0x33, 0x23, 0x33, 0x33, 0x3f, 0x3f, 0x3f, 0x3f, 0x3f, 0x3f, 0x3f, 0x33, 0x37, 0x3a,
        0x3a, 0x3a, 0x3a, 0x3a, 0x3a, 0x3a, 0x3a, 0x3a, 0xc6, 0x8b, 0xc5, 0xbc, 0x3a, 0x36,
    ];
    for input in [artifact_35, artifact_50] {
        let _ = decode(input);
    }
}

/// Duplicate keys are rejected.
#[test]
fn duplicate_key_is_rejected() {
    let result = decode(b"a = 1\na = 2\n");
    assert!(result.is_err());
}

/// Table redefinition is rejected.
#[test]
fn table_redefinition_is_rejected() {
    let result = decode(b"[a]\nx = 1\n[a]\ny = 2\n");
    assert!(result.is_err());
}

/// An implicit super-table created by `[fruit.apple]` may later be named by `[fruit]` — the spec's
/// valid-but-discouraged example.
#[test]
fn an_implicit_super_table_may_be_defined_explicitly() {
    let value =
        decode(b"[fruit.apple]\ncolor=\"red\"\n[fruit]\nname=\"apple\"\n").expect("the spec example must decode");
    let Value::Object(root) = value else {
        panic!("root is an object");
    };
    let Value::Object(fruit) = root.get("fruit").expect("fruit") else {
        panic!("fruit is a table");
    };
    let Value::String(name) = fruit.get("name").expect("name") else {
        panic!("name is a string");
    };
    assert_eq!(name.as_str(), "apple");
    let Value::Object(apple) = fruit.get("apple").expect("apple") else {
        panic!("apple is a table");
    };
    let Value::String(color) = apple.get("color").expect("color") else {
        panic!("color is a string");
    };
    assert_eq!(color.as_str(), "red");
    assert!(decode(b"a.b=1\n[a]\ny=2\n").is_err());
}

/// A table cannot be redefined as an array-of-tables (or vice versa): the name is already bound to one kind.
#[test]
fn table_versus_array_of_tables_conflict_is_rejected() {
    let result = decode(b"[a]\nx = 1\n[[a]]\ny = 2\n");
    assert!(result.is_err());
}

/// Opening a sub-table of an array-of-tables element IS valid TOML: `[a.b]` after `[[a]]` defines table `b` inside the
/// LAST element of `a`.
#[test]
fn sub_table_of_array_element_is_valid() {
    let value = decode(b"[[a]]\nx = 1\n[a.b]\ny = 2\n").expect("valid TOML");
    match &value {
        Value::Object(root) => {
            let a = root.get("a").expect("array-of-tables a");
            let Value::Array(array) = a else {
                panic!("a is an array")
            };
            assert_eq!(array.len(), 1);
            let Value::Object(element) = array.get(0).expect("first element") else {
                panic!("element is an object")
            };
            assert!(element.get("x").is_some());
            let b = element.get("b").expect("sub-table b");
            let Value::Object(b) = b else { panic!("b is an object") };
            assert!(b.get("y").is_some());
        }
        other => panic!("expected object root, got {other:?}"),
    }
}

/// Trailing content after the document is rejected.
#[test]
fn trailing_content_is_rejected() {
    let result = decode(b"a = 1\nb = 2 garbage");
    assert!(result.is_err());
}

/// A `#` comment following a value on the same line is valid TOML 1.0.0 for every value type, with any amount of
/// separating whitespace: only the byte offset of the comment (and whether more statements follow) should matter, never
/// the value's own kind.
#[test]
fn value_trailing_comment_is_accepted_for_every_value_type() {
    let cases: &[(&[u8], &str)] = &[
        (b"a = 1 # c\n", "1"),
        (b"a = 1# c\n", "1"),
        (b"a = 1   # c\n", "1"),
        (b"a = 1\t# c\n", "1"),
        (b"a = 1.5 # c\n", "1.5"),
        (b"a = \"s\" # c\n", "s"),
        (b"a = true # c\n", "true"),
        (b"a = [1, 2] # c\n", "[1, 2]"),
        (b"a = { x = 1 } # c\n", "{ x = 1 }"),
    ];
    for (bytes, describe) in cases {
        decode(bytes).unwrap_or_else(|error| panic!("value-trailing comment rejected for {describe}: {error:?}"));
    }
}

/// A value-trailing comment is legal in the MIDDLE of a document too — the statement loop must resume parsing the
/// next line, not treat the comment's consumption as satisfying the newline requirement twice.
#[test]
fn value_trailing_comment_permits_further_statements() {
    let value = decode(b"a = 1 # c\nb = 2\n").expect("decode");
    let Value::Object(object) = &value else {
        panic!("object")
    };
    assert!(object.get("a").is_some());
    assert!(object.get("b").is_some());
}

/// A comment after a table header must ALSO permit further statements — the same statement-end bug affected headers
/// whenever another statement followed on a later line.
#[test]
fn header_trailing_comment_permits_further_statements() {
    let value = decode(b"[t] # c\nx = 1\n").expect("decode");
    let Value::Object(root) = &value else { panic!("object") };
    let Value::Object(t) = root.get("t").expect("table t") else {
        panic!("t is an object")
    };
    assert!(t.get("x").is_some());
}

/// A realistic `Cargo.toml`-shaped fixture: value-trailing comments mixed with ordinary statements and a table header,
/// matching a real-world file.
#[test]
fn cargo_toml_shaped_fixture_with_trailing_comments_decodes() {
    let input = b"[package]\nname = \"demo\"\nversion = \"0.1.0\"        # bump on release\nedition = \"2024\" # keep in sync with rust-toolchain\n\n[dependencies]\nserde = { version = \"1\", features = [\"derive\"] } # pinned\n";
    let value = decode(input).expect("decode");
    let Value::Object(root) = &value else { panic!("object") };
    let Value::Object(package) = root.get("package").expect("package") else {
        panic!("package is an object")
    };
    assert!(matches!(package.get("version"), Some(Value::String(s)) if s.as_str() == "0.1.0"));
    let Value::Object(dependencies) = root.get("dependencies").expect("dependencies") else {
        panic!("dependencies is an object")
    };
    assert!(dependencies.get("serde").is_some());
}

/// A bare carriage return inside a value-trailing comment is still rejected (the comment grammar's own bare-CR law is
/// not weakened by this fix).
#[test]
fn value_trailing_comment_bare_cr_is_still_rejected() {
    let result = decode(b"a = 1 # c\rmore\n");
    assert!(result.is_err());
}

/// Genuine trailing garbage (not a comment) after a value is still rejected.
#[test]
fn value_trailing_garbage_is_still_rejected() {
    let result = decode(b"a = 1 garbage\n");
    assert!(result.is_err());
}

/// Empty input is a valid empty document.
#[test]
fn empty_document_is_valid() {
    let value = decode(b"").expect("empty decodes");
    match value {
        Value::Object(object) => assert_eq!(object.len(), 0),
        other => panic!("expected empty object, got {other:?}"),
    }
}

/// Raw C0 other than tab is rejected in every string spelling. Newline and carriage return keep their own per-spelling
/// laws; tab stays legal.
#[test]
fn raw_control_characters_in_strings_are_rejected() {
    let inputs: [(&[u8], &str); 8] = [
        (b"a = \"\x01\"\n", "basic string"),
        (b"a = '\x01'\n", "literal string"),
        (b"a = \"\\\"\n\x01\\\"\"\n", "multiline basic"),
        (b"a = '''\x01'''\n", "multiline literal"),
        (b"a = \"\x7F\"\n", "DEL in basic"),
        (b"a = \"\x00\"\n", "NUL in basic"),
        (b"a = \"\x0B\"\n", "vertical tab in basic"),
        (b"a = \"\x1F\"\n", "unit separator in basic"),
    ];
    for (input, kind) in inputs {
        let result = decode(input);
        assert!(result.is_err(), "{kind} accepted a raw control character");
    }
    // Tab is explicitly legal raw in every string spelling.
    assert!(decode(b"a = \"\t\"\n").is_ok(), "tab in basic string");
    assert!(decode(b"a = '''\t'''\n").is_ok(), "tab in multiline literal");
    // Escaped forms stay legal.
    assert!(decode(b"a = \"\\u0001\"\n").is_ok(), "escaped control");
}

/// The deterministic encoder round-trips a simple document.
#[test]
fn encoder_roundtrips_simple_document() {
    let out = decode_encode_roundtrip("a = 1\nb = \"x\"\n").expect("roundtrip");
    assert_eq!(out, "a = 1\nb = \"x\"\n");
}

/// `a.b = 1` normalizes to `[a]` + `b = 1`.
#[test]
fn encoder_normalizes_dotted_key() {
    let out = decode_encode_roundtrip("a.b = 1\n").expect("roundtrip");
    assert_eq!(out, "[a]\nb = 1\n");
}

/// A key the bare grammar refuses still emits as a quoted basic string, and a key of ASCII digits stays bare (TOML
/// reads it back as the string "1234").
#[test]
fn encoder_quotes_only_the_keys_the_bare_grammar_refuses() {
    let out = decode_encode_roundtrip("\"needs quotes\" = 1\n\"a.b\" = 2\n\"\" = 3\n1234 = 4\nok-_9 = 5\n")
        .expect("roundtrip");
    assert_eq!(
        out,
        "\"needs quotes\" = 1\n\"a.b\" = 2\n\"\" = 3\n1234 = 4\nok-_9 = 5\n"
    );
}

/// A quoted table-header component survives as a quoted component.
#[test]
fn encoder_quotes_a_table_header_component_that_cannot_be_bare() {
    // The intermediate table header is the encoder's own topology normalization; what this pins is that `a b` stays
    // quoted and `c` does not.
    let out = decode_encode_roundtrip("[\"a b\".c]\nk = 1\n").expect("roundtrip");
    assert_eq!(out, "[\"a b\"]\n\n[\"a b\".c]\nk = 1\n");
}

/// The encoder emits a blank line before each table header.
#[test]
fn encoder_blank_lines_between_tables() {
    let out = decode_encode_roundtrip("x = 1\n[table]\ny = 2\n").expect("roundtrip");
    assert_eq!(out, "x = 1\n\n[table]\ny = 2\n");
}

/// An empty root table emits zero bytes.
#[test]
fn encoder_empty_root_emits_zero_bytes() {
    let out = decode_encode_roundtrip("# comment only\n").expect("roundtrip");
    assert_eq!(out, "");
}

/// Encoding an unrepresentable value (null) fails.
#[test]
fn encoder_rejects_null() {
    let mut resources = common::resources();
    // Drive the encoder with an owned scalar root which is not a table.
    let bad = Value::Null;
    let result = encode(&bad, &mut resources);
    assert!(result.is_err());
}

/// TOML 1.0.0 caps integers at signed 64-bit: an exact integer one past `i64::MAX` has no valid TOML spelling, so the
/// encoder must DECLINE — never emit invalid bytes with a success outcome that its own decoder then rejects as
/// `integer out of range`.
#[test]
fn encoder_rejects_integer_beyond_i64_range() {
    let mut resources = common::resources();
    let mut object = jqf_data::Object::try_new().expect("object");
    let huge = jqf_data::Integer::parse("9223372036854775808").expect("parse i64::MAX + 1");
    let key = jqf_data::ObjectKey::try_from_str("n").expect("key");
    object
        .try_insert_unique(key, Value::Number(jqf_data::Number::integer(huge)))
        .expect("insert");
    let result = encode(&Value::Object(object), &mut resources);
    assert!(result.is_err(), "an out-of-i64-range integer must not encode to TOML");
}

/// The negative counterpart: one past `i64::MIN` is equally out of range.
#[test]
fn encoder_rejects_negative_integer_beyond_i64_range() {
    let mut resources = common::resources();
    let mut object = jqf_data::Object::try_new().expect("object");
    let huge = jqf_data::Integer::parse("-9223372036854775809").expect("parse i64::MIN - 1");
    let key = jqf_data::ObjectKey::try_from_str("n").expect("key");
    object
        .try_insert_unique(key, Value::Number(jqf_data::Number::integer(huge)))
        .expect("insert");
    let result = encode(&Value::Object(object), &mut resources);
    assert!(result.is_err());
}

/// An in-range boundary value (`i64::MAX` itself) still encodes normally — the fix must not become an off-by-one that
/// rejects the boundary too.
#[test]
fn encoder_still_accepts_i64_max() {
    let mut resources = common::resources();
    let mut object = jqf_data::Object::try_new().expect("object");
    let boundary = jqf_data::Integer::parse("9223372036854775807").expect("parse i64::MAX");
    let key = jqf_data::ObjectKey::try_from_str("n").expect("key");
    object
        .try_insert_unique(key, Value::Number(jqf_data::Number::integer(boundary)))
        .expect("insert");
    let out = encode(&Value::Object(object), &mut resources).expect("i64::MAX must encode");
    let text = String::from_utf8(out).expect("UTF-8 output");
    assert_eq!(text, "n = 9223372036854775807\n");
}

/// Encode → decode → compare over a corpus spanning every value type plus the fixes (trailing comments, radix
/// underscores, dotted keys inside inline tables): decoding, re-encoding, and decoding again must agree with the first
/// decode. A cheap semantic-equality property test that reuses the same breadth as the smoke corpus without a new
/// proptest dependency.
#[test]
fn decode_encode_decode_agrees_across_the_value_type_corpus() {
    let corpus = [
        "a = 1\n",
        "a = -1\n",
        "a = 1_000_000\n",
        "a = 0xff_ff\n",
        "a = 0o7_55\n",
        "a = 0b1101_0110\n",
        "a = 1.5\n",
        "a = 5.0\n",
        "a = inf\n",
        "a = -nan\n",
        "a = \"x\"\n",
        "a = true\n",
        "a = false\n",
        "a = [1, 2, 3]\n",
        "a = { x = 1, y = 2 }\n",
        "animal = { type.name = \"pug\" }\n",
        "a.b.c = 1\n",
        "[table]\nx = 1 # c\n",
        "[[a]]\nx = 1\n[[a]]\ny = 2\n",
        "a = 1979-05-27\n",
        "a = 07:32:00\n",
        "a = 1979-05-27T07:32:00Z\n",
        "a = 1 # trailing comment\n",
    ];
    for input in corpus {
        let first = decode(input.as_bytes()).unwrap_or_else(|error| {
            panic!("first decode of {input:?} failed: {error:?}");
        });
        let mut resources = common::resources();
        let encoded =
            encode(&first, &mut resources).unwrap_or_else(|error| panic!("encode of {input:?} failed: {error:?}"));
        let second = decode(&encoded).unwrap_or_else(|error| {
            panic!(
                "second decode of re-encoded {input:?} (encoded as {:?}) failed: {error:?}",
                String::from_utf8_lossy(&encoded)
            );
        });
        assert!(
            values_equal(&first, &second),
            "decode → encode → decode drifted for {input:?} (re-encoded as {:?})",
            String::from_utf8_lossy(&encoded)
        );
    }
}

/// An integer-valued decimal must round-trip as a FLOAT SPELLING, not silently become an integer: `5.0` encodes as
/// `5.0` (the `.0` suffix is mandatory) and decodes back to an exact decimal. This is the shortest-rendering law's TOML
/// consequence, numbers slice (a finite float spelling now decodes as an exact decimal).
#[test]
fn integer_valued_float_roundtrips_as_float() {
    let value = decode(b"a = 5.0\n").expect("decode");
    let Value::Object(object) = &value else {
        panic!("object")
    };
    let Value::Number(number) = object.get("a").expect("a") else {
        panic!("number")
    };
    assert!(number.as_decimal().is_some(), "5.0 must decode as a decimal");

    let out = decode_encode_roundtrip("a = 5.0\n").expect("roundtrip");
    assert_eq!(out, "a = 5.0\n");

    // The whole-valued decimal `1e15` keeps its exponent form.
    let out = decode_encode_roundtrip("a = 1e15\n").expect("roundtrip");
    assert_eq!(out, "a = 1E+15\n");
    let reparsed = decode(out.as_bytes()).expect("reparse");
    let Value::Object(object) = &reparsed else {
        panic!("object")
    };
    let Value::Number(number) = object.get("a").expect("a") else {
        panic!("number")
    };
    assert!(number.as_decimal().is_some(), "1e15 round-trips as a decimal");
}

/// A non-integer-valued float keeps its natural spelling.
#[test]
fn fractional_float_keeps_spelling() {
    let out = decode_encode_roundtrip("a = 1.5\nb = 0.1\n").expect("roundtrip");
    assert_eq!(out, "a = 1.5\nb = 0.1\n");
}

/// A nested table emits its FULL quoted path header, so the output reparses to the same structure (not a sibling table
/// at the root).
#[test]
fn nested_table_emits_full_path_header() {
    let out =
        decode_encode_roundtrip("[server]\nhost = \"example.org\"\n[server.tls]\nenabled = true\n").expect("roundtrip");
    assert_eq!(
        out,
        "[server]\nhost = \"example.org\"\n\n[server.tls]\nenabled = true\n"
    );
    // And the output reparses to the same nested structure.
    let reparsed = decode(out.as_bytes()).expect("reparse");
    let Value::Object(root) = &reparsed else {
        panic!("object")
    };
    let Value::Object(server) = root.get("server").expect("server") else {
        panic!("server")
    };
    let Value::Object(tls) = server.get("tls").expect("tls") else {
        panic!("tls")
    };
    assert!(tls.get("enabled").is_some());
}

/// TOML float grammar: digits are required on BOTH sides of the point and an exponent may stand alone (`1e5` is a
/// float). Trailing/leading-dot forms are rejected. A valid float spelling decodes as an exact decimal.
#[test]
fn float_grammar_is_enforced() {
    // `1e5` is a float (no fraction needed).
    let value = decode(b"a = 1e5\n").expect("1e5 is a valid float");
    let Value::Object(object) = &value else {
        panic!("object")
    };
    let Value::Number(number) = object.get("a").expect("a") else {
        panic!("number")
    };
    assert!(number.as_decimal().is_some());

    // Invalid: trailing dot, leading dot, empty fraction.
    let invalid: [&[u8]; 4] = [b"a = 1.\n", b"a = .5\n", b"a = 1.e5\n", b"a = +.5\n"];
    for bad in invalid {
        assert!(
            decode(bad).is_err(),
            "{:?} must be rejected",
            String::from_utf8_lossy(bad)
        );
    }
}

/// A bare scalar token that cannot begin any numeric spelling reports the missing value, never a float sub-part
/// message: the token reaches the number path (an `inf`/`nan` misspelling or sign-led letters), but it is not a float
/// attempt and must not be diagnosed as one — even when it contains an `e` that would otherwise route it through the
/// float validator.
#[test]
fn non_numeric_junk_tokens_do_not_report_float_subparts() {
    for bad in [
        &b"a = nope\n"[..],
        b"a = nano1\n",
        b"a = in\n",
        b"a = +hello\n",
        b"a = -abc\n",
        b"a = -e5\n",
        b"a = +\n",
    ] {
        let error = decode(bad).expect_err("junk token must be rejected");
        let message = error
            .diagnostic()
            .map(|diagnostic| diagnostic.message().to_owned())
            .expect("diagnostic");
        assert_eq!(
            message,
            "expected a value",
            "{:?} names what failed",
            String::from_utf8_lossy(bad)
        );
    }

    // Tokens that ARE float attempts keep their precise sub-part messages.
    let float_attempts: [(&[u8], &str); 4] = [
        (b"a = 1e\n", "invalid float exponent"),
        (b"a = 1ex\n", "invalid float exponent"),
        (b"a = 1.2.3\n", "invalid float fraction"),
        (b"a = +.5\n", "invalid float integer part"),
    ];
    for (bad, expected) in float_attempts {
        let error = decode(bad).expect_err("float attempt must be rejected");
        let message = error
            .diagnostic()
            .map(|diagnostic| diagnostic.message().to_owned())
            .expect("diagnostic");
        assert_eq!(
            message,
            expected,
            "{:?} keeps its part-specific message",
            String::from_utf8_lossy(bad)
        );
    }
}

/// The parse rejects invalid UTF-8.
#[test]
fn invalid_utf8_is_rejected() {
    let result = decode(&[b'a', b' ', b'=', b' ', 0xFF]);
    assert!(result.is_err());
}

/// The deterministic encoder renders the four temporal kinds and the output reparses to the same semantic values.
#[test]
fn temporals_roundtrip() {
    let out = decode_encode_roundtrip(
        "d1 = 1979-05-27\nt1 = 07:32:00\nodt = 1979-05-27T07:32:00Z\nldt = 1979-05-27T00:32:00.999999\n",
    )
    .expect("roundtrip");
    assert_eq!(
        out,
        "d1 = 1979-05-27\nt1 = 07:32:00\nodt = 1979-05-27T07:32:00Z\nldt = 1979-05-27T00:32:00.999999\n"
    );
    // The output reparses with the same temporal categories.
    let reparsed = decode(out.as_bytes()).expect("reparse");
    let Value::Object(object) = &reparsed else {
        panic!("object")
    };
    assert!(matches!(object.get("d1"), Some(Value::LocalDate(_))));
    assert!(matches!(object.get("t1"), Some(Value::LocalTime(_))));
    assert!(matches!(object.get("odt"), Some(Value::OffsetDateTime(_))));
    assert!(matches!(object.get("ldt"), Some(Value::LocalDateTime(_))));
}

/// Arrays of objects (an array-of-tables projected to an owned array) encode as TOML inline tables and reparse to the
/// same semantic value.
#[test]
fn array_of_tables_owned_roundtrip() {
    let out = decode_encode_roundtrip(
        "[[product]]\nname = \"Hammer\"\nsku = 738594937\n[[product]]\nname = \"Nail\"\nsku = 284758393\n",
    )
    .expect("roundtrip");
    assert_eq!(
        out,
        "product = [{name = \"Hammer\", sku = 738594937}, {name = \"Nail\", sku = 284758393}]\n"
    );
    let reparsed = decode(out.as_bytes()).expect("reparse");
    let Value::Object(object) = &reparsed else {
        panic!("object")
    };
    let Value::Array(array) = object.get("product").expect("product") else {
        panic!("product is an array")
    };
    assert_eq!(array.len(), 2);
}

/// Runs one decode, returning the storage statistics: which route each text took is observable through the document's
/// text storage.
fn decode_stats(bytes: &[u8]) -> jqf_data::DocumentTextStorageStats {
    let mut resources = common::resources();
    let registration = jqf_codec_toml::registration_1_0().expect("registration");
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_toml::TOML_JQF_1_0_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .expect("provider");
    let requirement = whole_requirement(&resources);
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources).expect("open");
    let result = {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run).expect("decode")
    };
    let AccessOutcome::FullDocument(product) = result.outcome() else {
        panic!("expected full document");
    };
    product.document().text_storage_stats().expect("text stats")
}

/// The source-span zero-copy route: verbatim strings, keys, and integers name their source bytes instead of the decoded
/// arena. Bare keys, literal strings, and zero-escape basic strings are verbatim; escaped strings, multiline strings,
/// and non-canonical integer spellings stay on the copying/render paths.
#[test]
fn verbatim_text_takes_the_sealed_source_route() {
    let stats = decode_stats(
        b"title = \"TOML\"\nlit = 'C:\\\\Users\\\\x'\ncount = 42\nneg = -7\nradix = 0x2A\nunderscored = 1_000\nsigned = +5\nescaped = \"a\\nb\"\n",
    );
    assert!(stats.trusted_session_source_attachment);
    // Keys: `title`, `lit`, `count`, `neg`, `radix`, `underscored`, `signed`, `escaped` — every one bare, so every
    // one source-backed.
    assert_eq!(stats.source_keys, 8);
    assert_eq!(stats.stored_keys, 0);
    // Values: `TOML` and the literal are source-backed; `a\nb` is escaped and must be decoded into the arena.
    assert_eq!(stats.source_string_values, 2);
    assert_eq!(stats.stored_string_values, 1);
    // Integers: `42` and `-7` are canonical spellings; `0x2A`, `1_000`, and `+5` canonicalize to different text and
    // render at build.
    assert_eq!(stats.source_integer_values, 2);
    assert_eq!(stats.stored_integer_refs, 5);
    // The arena holds the escaped string's decoded bytes plus the three rendered integer spellings (`42`, `1000`, `5`):
    // 3 + 2 + 4 + 1.
    assert_eq!(stats.decoded_arena_len, 10);
}

/// A document whose every text is copied (escaped or multiline) never binds the source: no seal, no attachment, and the
/// arena holds everything.
#[test]
fn fully_escaped_text_skips_source_sealing_and_attachment() {
    let stats = decode_stats(b"\"k\\u0065y\" = \"\\u0062\"\n\"mu\\u006cti\" = \"\"\"\nline\n\"\"\"\n");
    assert!(!stats.trusted_session_source_attachment);
    assert_eq!(stats.source_keys, 0);
    assert_eq!(stats.source_string_values, 0);
    assert_eq!(stats.source_integer_values, 0);
    assert_eq!(stats.stored_keys, 2);
    assert_eq!(stats.stored_string_values, 2);
    // `key`, `b`, `multi`, and the multiline body `line\n` (the opening newline is trimmed): 3 + 1 + 5 + 5.
    assert_eq!(stats.decoded_arena_len, 14);
}

/// Semantic equality over owned values (jqf's `Value` deliberately has no `PartialEq`: equality is a language operation
/// with its own laws, and the parity probe needs the plain structural one).
fn values_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => {
            a.to_integer() == b.to_integer()
                && a.as_float().map(jqf_data::Float::bits) == b.as_float().map(jqf_data::Float::bits)
        }
        (Value::String(a), Value::String(b)) => a.as_str() == b.as_str(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len()
                && (0..a.len()).all(|i| {
                    a.get(i)
                        .zip(b.get(i))
                        .is_some_and(|(left, right)| values_equal(left, right))
                })
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && (0..a.len()).all(|i| {
                    let key = a.get_index(i).map(jqf_data::ObjectEntry::key);
                    key.and_then(|key| b.get(key))
                        .is_some_and(|value| values_equal(a.get_index(i).expect("index").value(), value))
                })
        }
        (Value::LocalDate(a), Value::LocalDate(b)) => a == b,
        (Value::LocalTime(a), Value::LocalTime(b)) => a == b,
        (Value::LocalDateTime(a), Value::LocalDateTime(b)) => a == b,
        (Value::OffsetDateTime(a), Value::OffsetDateTime(b)) => {
            a.local.date == b.local.date && a.local.time == b.local.time && a.offset == b.offset
        }
        _ => false,
    }
}

/// Decodes with a forced container-span frontier and returns the materialized value plus the document's committed-span
/// count.
fn decode_with_frontier(bytes: &[u8], frontier: Option<u32>) -> Result<(Value, u32), CodecError> {
    let mut resources = common::resources();
    let registration = jqf_codec_toml::registration_1_0().expect("registration");
    let mut provider = registration.decoder().expect("decoder").create_provider(
        source(bytes),
        DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: &DialectId::try_new(jqf_codec_toml::TOML_JQF_1_0_DIALECT_ID).expect("dialect"),
            options: None,
            allow_adjacent_values: false,
            value_separator: &[],
        },
        &mut resources,
    )?;
    let mut requirement = whole_requirement(&resources);
    if let Some(depth) = frontier {
        requirement = requirement.with_lazy_frontier(depth);
    }
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources)?;
    let result = {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run)?
    };
    let AccessOutcome::FullDocument(product) = result.outcome() else {
        panic!("expected full document");
    };
    let spans = product.document().container_span_count();
    let value = product.document().materialize_root(&mut resources).map_err(|_| {
        CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "materialize TOML root",
        })
    })?;
    Ok((value, spans))
}

/// The container-span frontier's parity law: a lazy parse materializes to the identical value a frontier-off parse
/// does, across depths, and a touching program's answer over the deferred document is the eager answer. Value
/// materialization itself TOUCHES the deferred subtree (the span node materializes on read), so root materialization is
/// exactly the parity probe.
#[test]
fn forced_frontier_materializes_the_eager_value() {
    let input = b"ports = [8000, 8001, 8002]\npoint = { x = 1, y = 2 }\nname = \"plain\"\n";
    let (eager, eager_spans) = decode_with_frontier(input, None).expect("eager");
    assert_eq!(eager_spans, 0);
    for depth in [1, 2, 3] {
        let (lazy, lazy_spans) = decode_with_frontier(input, Some(depth)).expect("lazy at depth {depth}");
        // Depth 1 defers the two top-level containers; depths 2 and 3 cannot engage over a shape with no container
        // nested below them — the parity law holds at every depth, the engagement is shape-bound.
        if depth == 1 {
            assert!(lazy_spans > 0, "frontier 1 must commit the top-level container spans");
        } else {
            assert_eq!(lazy_spans, 0, "no container nests below depth 1 here");
        }
        assert!(
            values_equal(&lazy, &eager),
            "frontier {depth} diverged from the eager value"
        );
    }
}

/// A nested shape exercises the mixed built-outer/deferred-inner grammar transitions, and an escaped string inside a
/// deferred region must survive the owned round trip byte for byte.
#[test]
fn forced_frontier_parity_on_nested_and_escaped_shapes() {
    let input = b"nested = [[1, 2], [3, 4]]\ninner = { a = { b = \"x\\n\" } }\n";
    let (eager, _) = decode_with_frontier(input, None).expect("eager");
    for depth in [1, 2, 3] {
        let (lazy, spans) = decode_with_frontier(input, Some(depth)).expect("lazy");
        // Depths 1 and 2 defer respectively the top-level and the nested containers; nothing sits below depth 2, so
        // depth 3 cannot engage — and must still reproduce the eager value.
        if depth < 3 {
            assert!(spans > 0, "depth {depth} did not engage");
        } else {
            assert_eq!(spans, 0, "no container nests below depth 2 here");
        }
        assert!(
            values_equal(&lazy, &eager),
            "frontier {depth} diverged from the eager value"
        );
    }
    let Value::Object(object) = &eager else {
        panic!("expected an object");
    };
    let inner = object.get("inner").expect("inner");
    let Value::Object(inner) = inner else {
        panic!("expected inner object");
    };
    let Value::Object(a) = inner.get("a").expect("inner.a") else {
        panic!("expected inner.a object");
    };
    let Value::String(text) = a.get("b").expect("inner.a.b") else {
        panic!("expected inner.a.b string");
    };
    assert_eq!(text.as_str(), "x\n");
}

/// Decodes one document and returns the `DocumentProduct` (the located root view) so a test can inspect node source
/// spans.
fn decode_product(bytes: &[u8]) -> Result<jqf_codec_core::DocumentProduct<'_>, CodecError> {
    let mut resources = common::resources();
    let registration = jqf_codec_toml::registration_1_0().expect("registration");
    let mut provider = registration.decoder().expect("decoder").create_provider(
        source(bytes),
        DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: &DialectId::try_new(jqf_codec_toml::TOML_JQF_1_0_DIALECT_ID).expect("dialect"),
            options: None,
            allow_adjacent_values: false,
            value_separator: &[],
        },
        &mut resources,
    )?;
    let requirement = whole_requirement(&resources);
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources)?;
    let result = {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run)?
    };
    let AccessOutcome::FullDocument(product) = result.outcome() else {
        panic!("expected full document");
    };
    product.try_clone().map_err(|_| {
        CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "clone TOML product",
        })
    })
}

/// A token longer than the 64-byte stack buffer must not silently lose its first 64 bytes. Pre-fix, `TokenBuffer` kept
/// bytes 0..64 in the stack and 64.. in the heap and returned `heap` alone past 64, so a 70-digit integer became its
/// LAST 6 digits — an in-range value where TOML requires rejection.
#[test]
fn token_over_64_bytes_is_not_truncated_integer() {
    let seventy = "9".repeat(70);
    let input = format!("k = {seventy}\n");
    let error = decode(input.as_bytes()).expect_err("a 70-digit integer must reject");
    assert_eq!(error.kind(), CodecFailureKind::InvalidInput);
}

/// A bare key longer than 64 bytes keeps all of its bytes — pre-fix the decoded TEXT lost its first 64 bytes, so two
/// keys sharing the trailing bytes collided on their tails (the duplicate-key law rejected the second as a repeat of
/// the first).
#[test]
fn token_over_64_bytes_is_not_truncated_bare_key() {
    // Two 68-byte keys with distinct heads but the SAME 4-byte tail: pre-fix both truncated to `tail`, so the second
    // was a duplicate of the first.
    let tail = "same";
    let key_a = "a".repeat(64) + tail;
    let key_b = "b".repeat(64) + tail;
    let input = format!("{key_a} = 1\n{key_b} = 2\n");
    let Value::Object(object) = decode(input.as_bytes()).expect("long bare keys decode") else {
        panic!("expected an object");
    };
    assert_eq!(
        object.len(),
        2,
        "the two long keys must stay distinct (pre-fix they collided on the tail)"
    );
    let a = object.get(&key_a).expect("key a");
    let b = object.get(&key_b).expect("key b");
    assert!(values_equal(
        a,
        &Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(1)))
    ));
    assert!(values_equal(
        b,
        &Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(2)))
    ));
}

/// A long float token's committed source span covers the WHOLE token. Pre-fix the span bound `start + token.len()` saw
/// the truncated 6-byte token, so the encoder re-emitted the corrupted number from a 6-byte span.
#[test]
fn token_over_64_bytes_binds_the_full_span() {
    let mantissa = "1.".to_owned() + &"0".repeat(70);
    let input = format!("k = {mantissa}\n");
    let product = decode_product(input.as_bytes()).expect("long float decodes");
    let document = product.document();
    let token_start = 4_usize; // `k = `
    let token_end = token_start + mantissa.len();
    let mut spanned: Vec<(u32, u32)> = Vec::new();
    for index in 0..document.node_count() {
        let node = jqf_data::NodeId::try_from_index(index).expect("node index");
        if let Some(span) = document.node_source_span(node).expect("node source span") {
            spanned.push((span.start(), span.end()));
        }
    }
    assert!(
        spanned.iter().any(|&(start, end)| {
            start == u32::try_from(token_start).expect("start fits u32")
                && end == u32::try_from(token_end).expect("end fits u32")
        }),
        "no node binds the full token span {token_start}..{token_end}; spans: {spanned:?}"
    );
    assert!(
        !spanned
            .iter()
            .any(|&(_, end)| end == u32::try_from(token_start + 6).expect("fits u32")),
        "a truncated 6-byte span must not be committed; spans: {spanned:?}"
    );
}

fn decode_11(bytes: &[u8]) -> Result<Value, CodecError> {
    let mut resources = common::resources();
    let registration = jqf_codec_toml::registration_1_1().expect("registration");
    let mut provider = registration.decoder().expect("decoder").create_provider(
        source(bytes),
        DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: &DialectId::try_new(jqf_codec_toml::TOML_1_1_DIALECT_ID).expect("dialect"),
            options: None,
            allow_adjacent_values: false,
            value_separator: &[],
        },
        &mut resources,
    )?;
    let requirement = whole_requirement(&resources);
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources)?;
    let result = {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run)?
    };
    let AccessOutcome::FullDocument(product) = result.outcome() else {
        panic!("expected full document");
    };
    product.document().materialize_root(&mut resources).map_err(|_| {
        CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "materialize TOML root",
        })
    })
}

#[test]
fn utf8_bom_is_accepted() {
    let value = decode(b"\xEF\xBB\xBFa = 1\n").expect("BOM");
    let Value::Object(object) = value else {
        panic!("object");
    };
    assert!(matches!(object.get("a"), Some(Value::Number(_))));
}

#[test]
fn comment_forbidden_control_is_rejected() {
    assert!(decode(b"# \x00\na = 1\n").is_err());
    assert!(decode(b"# \x7F\na = 1\n").is_err());
}

#[test]
fn toml11_hex_byte_escape_decodes() {
    let value = decode_11(
        br#"a = "\x41"
"#,
    )
    .expect("\\x41");
    let Value::Object(object) = value else {
        panic!("object");
    };
    assert!(matches!(object.get("a"), Some(Value::String(s)) if s.as_str() == "A"));
}

#[test]
fn toml11_nbsp_is_not_a_bare_key() {
    assert!(decode_11(b"\xC2\xA0 = 1\n").is_err());
}
