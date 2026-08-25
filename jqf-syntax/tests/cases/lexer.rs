use jqf_syntax::{Lexer as SyntaxLexer, TokenKind};

fn lexer(source: &str) -> SyntaxLexer<'_> {
    SyntaxLexer::new(source).expect("test source fits compact syntax spans")
}

fn kinds(source: &str) -> Vec<TokenKind> {
    lexer(source).map(|token| token.kind).collect()
}

#[test]
fn lexer_skips_trivia_and_emits_one_eof_token() {
    assert_eq!(kinds("  # comment\n.  "), vec![TokenKind::Dot, TokenKind::Eof]);
}

#[test]
fn lexer_continues_comments_after_odd_trailing_backslashes() {
    assert_eq!(kinds("# comment\\\nnot_token\n."), vec![TokenKind::Dot, TokenKind::Eof]);
}

#[test]
fn lexer_continues_crlf_comments_after_odd_trailing_backslashes() {
    assert_eq!(kinds("# c\\\r\nnot_token\r\n."), vec![TokenKind::Dot, TokenKind::Eof]);
}

/// A comment ends at a line feed, and at a carriage return only when a line feed follows it. A lone carriage return is
/// ordinary comment text, so everything after it on the same physical line stays commented out.
#[test]
fn lexer_ends_a_comment_at_a_lone_carriage_return_only_when_a_line_feed_follows() {
    assert_eq!(kinds("# c\r."), vec![TokenKind::Eof]);
    assert_eq!(kinds("# c\r\n."), vec![TokenKind::Dot, TokenKind::Eof]);
    assert_eq!(kinds("# c\n."), vec![TokenKind::Dot, TokenKind::Eof]);
}

/// Exactly four bytes separate two tokens: space, tab, line feed and carriage return. The other ASCII whitespace bytes
/// — form feed and vertical tab — are invalid characters, each one error token, never silent trivia.
#[test]
fn lexer_separates_tokens_on_four_ascii_whitespace_bytes_only() {
    assert_eq!(
        kinds("1 +\t2\n*\r3"),
        vec![
            TokenKind::Number,
            TokenKind::Plus,
            TokenKind::Number,
            TokenKind::Star,
            TokenKind::Number,
            TokenKind::Eof,
        ]
    );
    for separator in ["\u{000c}", "\u{000b}"] {
        assert_eq!(
            kinds(&format!("1{separator}+{separator}2")),
            vec![
                TokenKind::Number,
                TokenKind::Error,
                TokenKind::Plus,
                TokenKind::Error,
                TokenKind::Number,
                TokenKind::Eof,
            ],
            "{separator:?}"
        );
    }
}

/// A number is the longest spelling that is actually a number: an `e`/`E` that no exponent follows belongs to whatever
/// comes next, so a keyword may begin immediately after a digit with no separator between them.
#[test]
fn lexer_gives_back_an_e_that_does_not_open_an_exponent() {
    assert_eq!(kinds("1end"), vec![TokenKind::Number, TokenKind::End, TokenKind::Eof]);
    assert_eq!(kinds("1else"), vec![TokenKind::Number, TokenKind::Else, TokenKind::Eof]);
    assert_eq!(kinds("1elif"), vec![TokenKind::Number, TokenKind::Elif, TokenKind::Eof]);
    assert_eq!(
        kinds("1e+"),
        vec![TokenKind::Number, TokenKind::Ident, TokenKind::Plus, TokenKind::Eof]
    );
    // A complete exponent is still one number, sign and all.
    for exponent in ["1e5", "1E5", "1e+5", "1E-2", "1.5e10"] {
        assert_eq!(kinds(exponent), vec![TokenKind::Number, TokenKind::Eof], "{exponent:?}");
    }
}

#[test]
fn lexer_prefers_longest_fixed_token_match() {
    assert_eq!(
        kinds("?// //= == != .@ .& .. ."),
        vec![
            TokenKind::DestructureAlt,
            TokenKind::AltAssign,
            TokenKind::Eq,
            TokenKind::Ne,
            TokenKind::DotAt,
            TokenKind::DotAmp,
            TokenKind::DotDot,
            TokenKind::Dot,
            TokenKind::Eof,
        ]
    );
}

#[test]
fn lexer_does_not_recognize_javascript_strict_equality_spellings() {
    assert_eq!(kinds("==="), vec![TokenKind::Eq, TokenKind::Assign, TokenKind::Eof]);
    assert_eq!(kinds("!=="), vec![TokenKind::Ne, TokenKind::Assign, TokenKind::Eof]);
}

#[test]
fn lexer_dispatch_covers_the_complete_fixed_token_inventory() {
    for kind in TokenKind::ALL {
        if let Some(lexeme) = kind.fixed_lexeme() {
            assert_eq!(
                kinds(lexeme),
                vec![*kind, TokenKind::Eof],
                "fixed-token dispatcher omitted {kind:?} ({lexeme:?})"
            );
        }
    }
}

#[test]
fn lexer_recognizes_every_reserved_keyword_spelling() {
    let keywords = [
        ("and", TokenKind::And),
        ("or", TokenKind::Or),
        ("as", TokenKind::As),
        ("def", TokenKind::Def),
        ("module", TokenKind::Module),
        ("import", TokenKind::Import),
        ("include", TokenKind::Include),
        ("if", TokenKind::If),
        ("then", TokenKind::Then),
        ("elif", TokenKind::Elif),
        ("else", TokenKind::Else),
        ("end", TokenKind::End),
        ("try", TokenKind::Try),
        ("catch", TokenKind::Catch),
        ("reduce", TokenKind::Reduce),
        ("foreach", TokenKind::Foreach),
        ("label", TokenKind::Label),
        ("break", TokenKind::Break),
        ("let", TokenKind::Let),
        ("empty", TokenKind::Empty),
        ("null", TokenKind::Null),
        ("true", TokenKind::True),
        ("false", TokenKind::False),
    ];
    for (spelling, kind) in keywords {
        assert_eq!(kinds(spelling), vec![kind, TokenKind::Eof], "{spelling:?}");
    }
    // A keyword spelling inside a longer name is still a plain identifier.
    assert_eq!(kinds("iftrue"), vec![TokenKind::Ident, TokenKind::Eof]);
}

#[test]
fn lexer_treats_non_ascii_whitespace_as_invalid_lexemes_not_trivia() {
    // Trivia is ASCII whitespace only: U+00A0 and U+2028 do not separate tokens, and each non-ASCII whitespace
    // character lexes as one Error token rather than being skipped.
    assert_eq!(
        kinds("a\u{00A0}b\u{2028}c"),
        vec![
            TokenKind::Ident,
            TokenKind::Error,
            TokenKind::Ident,
            TokenKind::Error,
            TokenKind::Ident,
            TokenKind::Eof,
        ]
    );
}

#[test]
fn lexer_recognizes_names_keywords_variables_and_formats() {
    assert_eq!(
        kinds("foo let and empty $name @json"),
        vec![
            TokenKind::Ident,
            TokenKind::Let,
            TokenKind::And,
            TokenKind::Empty,
            TokenKind::Variable,
            TokenKind::Format,
            TokenKind::Eof,
        ]
    );
}

#[test]
fn lexer_keeps_invalid_variable_spellings_in_one_error_token() {
    let tokens = lexer("$ $1 $::name $$$$var $foo:: $foo::1 $foo::$bar").collect::<Vec<_>>();

    assert_eq!(
        tokens.iter().map(|token| token.kind).collect::<Vec<_>>(),
        vec![
            TokenKind::Error,
            TokenKind::Error,
            TokenKind::Error,
            TokenKind::Error,
            TokenKind::Error,
            TokenKind::Error,
            TokenKind::Error,
            TokenKind::Eof,
        ]
    );
    assert_eq!(tokens[0].span.start(), 0);
    assert_eq!(tokens[0].span.end(), 1);
    assert_eq!(tokens[1].span.start(), 2);
    assert_eq!(tokens[1].span.end(), 4);
    assert_eq!(tokens[2].span.start(), 5);
    assert_eq!(tokens[2].span.end(), 12);
    assert_eq!(tokens[3].span.start(), 13);
    assert_eq!(tokens[3].span.end(), 20);
    assert_eq!(tokens[4].span.start(), 21);
    assert_eq!(tokens[4].span.end(), 27);
    assert_eq!(tokens[5].span.start(), 28);
    assert_eq!(tokens[5].span.end(), 35);
    assert_eq!(tokens[6].span.start(), 36);
    assert_eq!(tokens[6].span.end(), 46);
}

#[test]
fn lexer_preserves_spans_for_tokens() {
    let tokens = lexer(" .foo").collect::<Vec<_>>();

    assert_eq!(tokens[0].kind, TokenKind::Dot);
    assert_eq!(tokens[0].span.start(), 1);
    assert_eq!(tokens[0].span.end(), 2);
    assert_eq!(tokens[1].kind, TokenKind::Ident);
    assert_eq!(tokens[1].span.start(), 2);
    assert_eq!(tokens[1].span.end(), 5);
}

#[test]
fn lexer_recognizes_numeric_forms_without_absorbing_minus() {
    assert_eq!(
        kinds("-1 -.5 .5 1. 1.e2 .5e2"),
        vec![
            TokenKind::Minus,
            TokenKind::Number,
            TokenKind::Minus,
            TokenKind::Number,
            TokenKind::Number,
            TokenKind::Number,
            TokenKind::Number,
            TokenKind::Number,
            TokenKind::Eof,
        ]
    );
}

#[test]
fn lexer_recognizes_strings_and_marks_unterminated_strings_as_errors() {
    assert_eq!(
        kinds(r#""plain" "template \(.)" "unterminated"#),
        vec![TokenKind::String, TokenKind::String, TokenKind::Error, TokenKind::Eof,]
    );
}

#[test]
fn lexer_ignores_comment_syntax_inside_string_interpolation() {
    let source = "\"\\(1 # comment \" (\n + 2)\"";

    assert_eq!(kinds(source), vec![TokenKind::String, TokenKind::Eof]);
}

#[test]
fn lexer_follows_interpolations_nested_inside_interpolated_strings() {
    for source in [
        r#""\("\(")")")""#,
        r#""\("\("\"")")""#,
        r#""\("\(1)")""#,
        r#""\("\("x")" + "\(2)")""#,
    ] {
        assert_eq!(kinds(source), vec![TokenKind::String, TokenKind::Eof], "{source:?}");
    }

    // An interpolation left open inside a nested string never closes the outer string either.
    assert_eq!(kinds(r#""\("\(")")""#), vec![TokenKind::Error, TokenKind::Eof]);
}

/// A closed quote with an unclosed `[`/`{` inside the interpolation is still a string token. The missing bracket is the
/// interpolation's diagnostic.
#[test]
fn lexer_ends_a_string_when_interpolation_closes_with_unclosed_brackets() {
    assert_eq!(kinds(r#""\({a:1)""#), vec![TokenKind::String, TokenKind::Eof]);
    assert_eq!(kinds(r#""\([1)""#), vec![TokenKind::String, TokenKind::Eof]);
    assert_eq!(kinds(r#""\(.name + "x")""#), vec![TokenKind::String, TokenKind::Eof]);
}
