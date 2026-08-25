use core::mem::size_of;

use jqf_source::{LabelStyle, Severity, SourceId, SourceKind, SourceRef, Span};
use jqf_syntax::{ExpectedTokens, GrammarContext, SyntaxErrorKind, TokenKind, parse_query as try_parse_query};

#[test]
fn syntax_error_kinds_have_stable_codes() {
    assert_eq!(SyntaxErrorKind::InvalidToken.code().to_string(), "syntax.invalid-token");
    assert_eq!(
        SyntaxErrorKind::InvalidVariable.code().to_string(),
        "syntax.invalid-variable"
    );
    assert_eq!(
        SyntaxErrorKind::UnterminatedString.code().to_string(),
        "syntax.unterminated-string"
    );
    assert_eq!(
        SyntaxErrorKind::SeparatedAccessor.code().to_string(),
        "syntax.separated-accessor"
    );
    assert_eq!(
        SyntaxErrorKind::ExpectedExpression.code().to_string(),
        "syntax.expected-expression"
    );
    assert_eq!(
        SyntaxErrorKind::UnexpectedToken.code().to_string(),
        "syntax.unexpected-token"
    );
    assert_eq!(
        SyntaxErrorKind::ChainedAssignment.code().to_string(),
        "syntax.chained-assignment"
    );
    assert_eq!(
        SyntaxErrorKind::MalformedObjectKey.code().to_string(),
        "syntax.malformed-object-key"
    );
    assert_eq!(
        SyntaxErrorKind::MissingBreakLabel.code().to_string(),
        "syntax.missing-break-label"
    );
    assert_eq!(
        SyntaxErrorKind::ExpectedToken {
            expected: ExpectedTokens::new(&[TokenKind::RParen]),
            context: GrammarContext::Group,
        }
        .code()
        .to_string(),
        "syntax.expected-token"
    );
}

#[test]
fn syntax_error_kinds_build_primary_error_diagnostics() {
    let source = SourceRef::new(SourceId::new(7), SourceKind::Query);
    let span = Span::new(3, 4);
    let diagnostic = SyntaxErrorKind::InvalidToken.diagnostic(source, span);

    assert_eq!(diagnostic.code().to_string(), "syntax.invalid-token");
    assert_eq!(diagnostic.severity(), Severity::Error);
    assert_eq!(diagnostic.message(), "invalid token");
    assert_eq!(diagnostic.labels().len(), 1);
    assert_eq!(diagnostic.labels()[0].source(), source);
    assert_eq!(diagnostic.labels()[0].span(), span);
    assert_eq!(diagnostic.labels()[0].message(), "this token is not valid jqf syntax");
}

#[test]
fn expected_token_diagnostics_name_the_expected_token() {
    let source = SourceRef::new(SourceId::new(8), SourceKind::Query);
    let span = Span::new(10, 10);
    let diagnostic = SyntaxErrorKind::ExpectedToken {
        expected: ExpectedTokens::new(&[TokenKind::RBrace]),
        context: GrammarContext::Object,
    }
    .diagnostic(source, span);

    assert_eq!(diagnostic.message(), "expected closing brace in object expression");
    assert_eq!(diagnostic.labels()[0].message(), "expected closing brace here");
}

#[test]
fn expected_tokens_are_fixed_compact_ordered_storage() {
    let expected = ExpectedTokens::new(&[
        TokenKind::Ident,
        TokenKind::String,
        TokenKind::Variable,
        TokenKind::Format,
        TokenKind::LParen,
    ]);

    assert!(size_of::<ExpectedTokens>() <= 8);
    assert_eq!(size_of::<GrammarContext>(), 1);
    assert_eq!(
        expected.as_slice(),
        &[
            TokenKind::Ident,
            TokenKind::String,
            TokenKind::Variable,
            TokenKind::Format,
            TokenKind::LParen,
        ]
    );
    assert!(expected.as_slice().contains(&TokenKind::Format));
    assert!(!expected.as_slice().contains(&TokenKind::RBrace));

    let truncated = ExpectedTokens::new(&[
        TokenKind::Ident,
        TokenKind::String,
        TokenKind::Variable,
        TokenKind::Format,
        TokenKind::LParen,
        TokenKind::RBrace,
    ]);
    assert_eq!(truncated.as_slice(), expected.as_slice());
}

#[test]
fn multiple_expectations_include_grammar_context() {
    let source = SourceRef::new(SourceId::new(8), SourceKind::Query);
    let span = Span::new(10, 10);
    let diagnostic = SyntaxErrorKind::ExpectedToken {
        expected: ExpectedTokens::new(&[TokenKind::Semi, TokenKind::RParen]),
        context: GrammarContext::Call,
    }
    .diagnostic(source, span);

    assert_eq!(
        diagnostic.message(),
        "expected semicolon or closing parenthesis in function call"
    );
    assert_eq!(
        diagnostic.labels()[0].message(),
        "expected semicolon or closing parenthesis here"
    );
}

#[test]
fn three_expectations_render_alternative_list_without_an_oxford_comma() {
    let source = SourceRef::new(SourceId::new(8), SourceKind::Query);
    let diagnostic = SyntaxErrorKind::ExpectedToken {
        expected: ExpectedTokens::new(&[TokenKind::Elif, TokenKind::Else, TokenKind::End]),
        context: GrammarContext::Conditional,
    }
    .diagnostic(source, Span::new(10, 10));

    assert_eq!(
        diagnostic.message(),
        "expected elif keyword, else keyword or end keyword in conditional expression"
    );
    assert_eq!(
        diagnostic.labels()[0].message(),
        "expected elif keyword, else keyword or end keyword here"
    );
}

#[test]
fn four_expectations_render_comma_separated_alternatives() {
    let source = SourceRef::new(SourceId::new(8), SourceKind::Query);
    let diagnostic = SyntaxErrorKind::ExpectedToken {
        expected: ExpectedTokens::new(&[
            TokenKind::Ident,
            TokenKind::String,
            TokenKind::Variable,
            TokenKind::LParen,
        ]),
        context: GrammarContext::Pattern,
    }
    .diagnostic(source, Span::new(10, 10));

    assert_eq!(
        diagnostic.message(),
        "expected identifier, string literal or template, variable or opening \
         parenthesis in binding pattern"
    );
    assert_eq!(
        diagnostic.labels()[0].message(),
        "expected identifier, string literal or template, variable or opening \
         parenthesis here"
    );
}

#[test]
fn five_expectations_render_the_pattern_member_candidates() {
    let source = SourceRef::new(SourceId::new(8), SourceKind::Query);
    let diagnostic = SyntaxErrorKind::ExpectedToken {
        expected: ExpectedTokens::new(&[
            TokenKind::Ident,
            TokenKind::String,
            TokenKind::Variable,
            TokenKind::Format,
            TokenKind::LParen,
        ]),
        context: GrammarContext::Pattern,
    }
    .diagnostic(source, Span::new(10, 10));

    assert_eq!(
        diagnostic.message(),
        "expected identifier, string literal or template, variable, format \
         filter or opening parenthesis in binding pattern"
    );
    assert_eq!(
        diagnostic.labels()[0].message(),
        "expected identifier, string literal or template, variable, format \
         filter or opening parenthesis here"
    );
}

#[test]
fn unclosed_delimiters_carry_a_secondary_opener_label() {
    let source = SourceRef::new(SourceId::new(8), SourceKind::Query);
    let diagnostic = SyntaxErrorKind::UnclosedDelimiter {
        expected: TokenKind::RParen,
        context: GrammarContext::Group,
    }
    .diagnostic_with_opener(source, Span::new(7, 7), Span::new(0, 1));

    assert_eq!(diagnostic.message(), "unclosed group; expected closing parenthesis");
    assert_eq!(diagnostic.labels().len(), 2);
    assert_eq!(diagnostic.labels()[0].style(), LabelStyle::Primary);
    assert_eq!(diagnostic.labels()[0].span(), Span::new(7, 7));
    assert_eq!(diagnostic.labels()[1].style(), LabelStyle::Secondary);
    assert_eq!(diagnostic.labels()[1].span(), Span::new(0, 1));
    assert_eq!(diagnostic.labels()[1].message(), "group starts here");
}

#[test]
fn focused_grammar_failures_have_stable_diagnostics() {
    let cases = [
        ("(", vec!["syntax.expected-expression", "syntax.unclosed-delimiter"]),
        ("if . then .", vec!["syntax.unterminated-control"]),
        ("{: .}", vec!["syntax.malformed-object-key"]),
        ("break", vec!["syntax.missing-break-label"]),
        (".a = .b = .c", vec!["syntax.chained-assignment"]),
        ("reduce . ($x; .)", vec!["syntax.expected-token"]),
        ("def : .; .", vec!["syntax.expected-token"]),
        (
            "call(;)",
            vec!["syntax.expected-call-argument", "syntax.expected-call-argument"],
        ),
        (". as [,] | .", vec!["syntax.expected-token", "syntax.unexpected-token"]),
        ("try . catch", vec!["syntax.unterminated-control"]),
    ];

    for (query, expected_codes) in cases {
        let parsed = parse(query);
        let codes: Vec<_> = parsed
            .diagnostics()
            .iter()
            .map(|diagnostic| diagnostic.code().to_string())
            .collect();
        assert_eq!(codes, expected_codes, "{query:?}: diagnostics moved");
    }
}

#[test]
fn every_grammar_context_renders_its_description_and_opener_label() {
    let source = SourceRef::new(SourceId::new(8), SourceKind::Query);
    let span = Span::new(10, 10);
    let cases = [
        (GrammarContext::Expression, "expression", "expression starts here"),
        (GrammarContext::Group, "group", "group starts here"),
        (GrammarContext::Array, "array expression", "array starts here"),
        (GrammarContext::Object, "object expression", "object starts here"),
        (GrammarContext::Index, "index expression", "index starts here"),
        (GrammarContext::Call, "function call", "call starts here"),
        (
            GrammarContext::NodeAccessor,
            "node accessor",
            "node accessor starts here",
        ),
        (
            GrammarContext::AttributeAccessor,
            "attribute accessor",
            "attribute accessor starts here",
        ),
        (GrammarContext::Pattern, "binding pattern", "pattern starts here"),
        (GrammarContext::Let, "let expression", "let expression starts here"),
        (
            GrammarContext::EngineSurface,
            "engine-surface term",
            "engine-surface term starts here",
        ),
        (
            GrammarContext::Conditional,
            "conditional expression",
            "conditional starts here",
        ),
        // The `try` opener label names the catch, the component the authored opener points at when it is missing.
        (GrammarContext::Try, "try expression", "catch starts here"),
        (
            GrammarContext::Reduce,
            "reduce expression",
            "reduce expression starts here",
        ),
        (
            GrammarContext::Foreach,
            "foreach expression",
            "foreach expression starts here",
        ),
        (
            GrammarContext::Label,
            "label expression",
            "label expression starts here",
        ),
        (
            GrammarContext::Definition,
            "function definition",
            "definition starts here",
        ),
        (GrammarContext::SourceItem, "source item", "source item starts here"),
    ];
    for (context, description, opener_label) in cases {
        let diagnostic = SyntaxErrorKind::ExpectedToken {
            expected: ExpectedTokens::new(&[TokenKind::RParen]),
            context,
        }
        .diagnostic(source, span);
        assert_eq!(
            diagnostic.message(),
            format!("expected closing parenthesis in {description}"),
            "{context:?}"
        );
        assert_eq!(
            diagnostic.labels()[0].message(),
            "expected closing parenthesis here",
            "{context:?}"
        );

        let opener = SyntaxErrorKind::UnclosedDelimiter {
            expected: TokenKind::RParen,
            context,
        }
        .diagnostic_with_opener(source, span, span);
        assert_eq!(opener.labels()[1].message(), opener_label, "{context:?}");
    }
}

#[test]
fn catchless_try_remains_valid() {
    assert!(parse("try .").into_valid_syntax().is_ok());
}

/// A lexer error token is classified from its own spelling, so a caller reads what went wrong and not just that
/// something did.
///
/// The lexer emits one undifferentiated error token; the classification is the parser's, and it is what separates an
/// unfinished string from a malformed variable from an accessor its dot no longer touches. Anything else stays
/// `invalid-token`.
#[test]
fn a_lexer_error_is_reported_as_the_shape_it_failed_to_be() {
    for (query, code) in [
        (r#""unfinished"#, "syntax.unterminated-string"),
        (r#". + "no closing quote"#, "syntax.unterminated-string"),
        (r#""\({a:1)""#, "syntax.unclosed-delimiter"),
        (r#""\([1)""#, "syntax.unclosed-delimiter"),
        ("$", "syntax.invalid-variable"),
        ("$1", "syntax.invalid-variable"),
        (". as $ | .", "syntax.invalid-variable"),
        ("$a::", "syntax.invalid-variable"),
        (". &href", "syntax.separated-accessor"),
        (". @", "syntax.separated-accessor"),
        ("^", "syntax.invalid-token"),
        ("a \u{00A0} b", "syntax.invalid-token"),
    ] {
        let parsed = parse(query);
        let codes: Vec<_> = parsed
            .diagnostics()
            .iter()
            .map(|diagnostic| diagnostic.code().to_string())
            .collect();
        assert_eq!(codes.first().map(String::as_str), Some(code), "{query:?}");
    }
}

/// An empty binding pattern reports at the closer with a secondary label back at the authored opener, like every other
/// delimiter diagnostic.
#[test]
fn empty_patterns_carry_a_secondary_opener_label() {
    for query in [". as [] | .", ". as {} | ."] {
        let parsed = parse(query);
        let diagnostics = parsed.diagnostics();
        assert_eq!(diagnostics.len(), 1, "{query:?}: {diagnostics:?}");
        let diagnostic = &diagnostics[0];
        assert_eq!(diagnostic.code().to_string(), "syntax.expected-expression");
        let labels = diagnostic.labels();
        assert_eq!(labels.len(), 2, "{query:?}");
        assert_eq!(labels[0].style(), LabelStyle::Primary);
        assert_eq!(labels[0].span(), Span::new(6, 7), "{query:?}: closer");
        assert_eq!(labels[1].style(), LabelStyle::Secondary);
        assert_eq!(labels[1].span(), Span::new(5, 6), "{query:?}: opener");
    }
}

fn parse(query: &str) -> jqf_syntax::Parse<jqf_syntax::Expr> {
    try_parse_query(SourceRef::new(SourceId::new(9), SourceKind::Query), query).unwrap()
}
