use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};
use jqf_syntax::{SyntaxSourceError, parse_library, parse_program, parse_query};

fn source() -> SourceRef {
    SourceRef::new(SourceId::new(41), SourceKind::Query)
}

#[test]
fn parsed_syntax_retains_query_metadata_and_binds_interpolation_source() {
    let text = r#""price=\(.price.@tag)""#;
    let parsed = parse_query(source(), text).expect("test source fits compact spans");
    let syntax = parsed.syntax().expect("query should recover a root");

    assert_eq!(syntax.source_ref(), source());
    assert_eq!(syntax.source_len(), u32::try_from(text.len()).unwrap());
    assert_eq!(syntax.root().span().range(), 0..text.len());

    let bound = syntax
        .bind(ResolvedSource::new(source(), "query.jq", text.as_bytes(), 17))
        .expect("matching UTF-8 source should bind");
    assert_eq!(bound.root().span().range(), 0..text.len());
    assert_eq!(bound.source().source_ref(), source());
    assert_eq!(bound.source().label(), "query.jq");
    assert_eq!(bound.source().text(), text);
}

#[test]
fn source_unit_recovery_wrappers_and_roots_are_preserved() {
    parse_program(source(), "def id: .; id")
        .unwrap()
        .into_valid_syntax()
        .expect("program should be valid");
    let library = parse_library(source(), "def id: .;")
        .unwrap()
        .into_valid_syntax()
        .expect("library should be valid");
    assert!(library.root().expression.is_none());

    let recovered = parse_query(source(), ". @tag").unwrap();
    assert!(recovered.syntax().is_some(), "recovery root");
    assert!(!recovered.diagnostics().is_empty());

    let root = parse_query(source(), ".")
        .unwrap()
        .into_valid_syntax()
        .expect("query should be valid")
        .into_root();
    assert_eq!(root.span().range(), 0..1);
}

#[test]
fn source_binding_rejects_mismatched_wrong_length_and_non_utf8_sources() {
    let parsed = parse_query(source(), ".").unwrap();
    let syntax = parsed.syntax().unwrap();

    assert_eq!(
        syntax.bind(ResolvedSource::new(
            SourceRef::new(SourceId::new(42), SourceKind::Query),
            "other.jq",
            b".",
            0,
        )),
        Err(SyntaxSourceError::SourceMismatch)
    );
    assert_eq!(
        syntax.bind(ResolvedSource::new(source(), "query.jq", b"..", 0)),
        Err(SyntaxSourceError::LengthMismatch)
    );
    // The refusal keeps the offset of the first invalid byte: `0xff` is invalid on its own, so nothing before it is
    // valid text.
    assert_eq!(
        syntax.bind(ResolvedSource::new(source(), "query.jq", &[0xff], 0)),
        Err(SyntaxSourceError::NonUtf8 { valid_up_to: 0 })
    );
}
