use jqf_source::{SourceId, SourceKind, SourceRef};
use jqf_syntax::{Lexer, MAX_SYNTAX_SOURCE_BYTES, SyntaxInputError, parse_library, parse_program, parse_query};

fn source() -> SourceRef {
    SourceRef::new(SourceId::new(37), SourceKind::Query)
}

#[test]
fn syntax_input_boundary_is_public_and_descriptive() {
    assert_eq!(MAX_SYNTAX_SOURCE_BYTES, u32::MAX as usize);

    let error = SyntaxInputError::SourceTooLarge {
        limit: MAX_SYNTAX_SOURCE_BYTES,
        attempted: MAX_SYNTAX_SOURCE_BYTES.saturating_add(1),
    };
    assert!(error.to_string().contains("exceeding"));
}

#[test]
fn every_entry_route_admits_representable_source() {
    assert!(Lexer::new(".").is_ok());
    assert!(parse_query(source(), ".").is_ok());
    assert!(parse_program(source(), ".").is_ok());
    assert!(parse_library(source(), "def id: .;").is_ok());
}
