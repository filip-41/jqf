use jqf_source::Span;
use jqf_syntax::{Token, TokenKind};

#[test]
fn token_preserves_kind_and_span() {
    let span = Span::from_usize(2, 5);
    let token = Token::new(TokenKind::Ident, span);

    assert_eq!(token.kind, TokenKind::Ident);
    assert_eq!(token.span, span);
}

#[test]
fn fixed_lexemes_cover_operator_tokens() {
    let cases = [
        (TokenKind::DestructureAlt, "?//"),
        (TokenKind::AltAssign, "//="),
        (TokenKind::PipeAssign, "|="),
        (TokenKind::DoubleColon, "::"),
        (TokenKind::DotAt, ".@"),
        (TokenKind::DotAmp, ".&"),
        (TokenKind::Tilde, "~"),
        (TokenKind::Eq, "=="),
        (TokenKind::Ne, "!="),
        (TokenKind::Alt, "//"),
        (TokenKind::Dot, "."),
    ];

    for (kind, lexeme) in cases {
        assert_eq!(kind.fixed_lexeme(), Some(lexeme), "{kind:?}");
    }
}

#[test]
fn non_fixed_tokens_do_not_report_fixed_lexemes() {
    for kind in [
        TokenKind::Ident,
        TokenKind::Variable,
        TokenKind::Number,
        TokenKind::String,
        TokenKind::Format,
        TokenKind::Let,
        TokenKind::Eof,
        TokenKind::Error,
    ] {
        assert_eq!(kind.fixed_lexeme(), None, "{kind:?}");
    }
}

#[test]
fn token_kinds_describe_source_forms() {
    assert_eq!(TokenKind::Eq.description(), "semantic equality operator");
    assert_eq!(TokenKind::DotAt.description(), "node accessor introducer");
    assert_eq!(TokenKind::DotAmp.description(), "named attribute introducer");
    assert_eq!(TokenKind::Let.description(), "let keyword");
}

#[test]
fn stable_token_inventory_is_unique_and_described() {
    for (index, kind) in TokenKind::ALL.iter().enumerate() {
        assert!(!kind.description().is_empty(), "{kind:?} must have a description");
        assert!(
            !TokenKind::ALL[..index].contains(kind),
            "{kind:?} appears more than once in TokenKind::ALL"
        );
    }
}
