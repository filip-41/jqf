use jqf_source::{Diagnostic, SourceId, SourceKind, SourceRef, Span};
use jqf_syntax::{Lexer, TokenKind};

use super::common::{parse_library, parse_program, parse_query};

fn source_ref() -> SourceRef {
    SourceRef::new(SourceId::new(31), SourceKind::Query)
}

fn assert_span(source: &str, span: Span) {
    assert!(span.start() <= span.end(), "{span} in {source:?}");
    assert!(
        usize::try_from(span.end()).unwrap() <= source.len(),
        "{span} in {source:?}"
    );
    assert!(
        source.is_char_boundary(usize::try_from(span.start()).unwrap()),
        "{span} starts inside a character in {source:?}"
    );
    assert!(
        source.is_char_boundary(usize::try_from(span.end()).unwrap()),
        "{span} ends inside a character in {source:?}"
    );
}

fn assert_diagnostics(source: &str, diagnostics: &[Diagnostic]) {
    for diagnostic in diagnostics {
        for label in diagnostic.labels() {
            assert_span(source, label.span());
        }
    }
}

fn next(state: &mut u32) -> u32 {
    *state ^= state.wrapping_shl(13);
    *state ^= state.wrapping_shr(17);
    *state ^= state.wrapping_shl(5);
    *state
}

#[test]
fn deterministic_malformed_unicode_source_never_panics_or_emits_invalid_utf8_boundaries() {
    const PIECES: &[&str] = &[
        ".",
        "..",
        ".@",
        ".&",
        "?",
        "?//",
        "|",
        ",",
        ":",
        ";",
        "::",
        "(",
        ")",
        "[",
        "]",
        "{",
        "}",
        "+",
        "-",
        "*",
        "/",
        "%",
        "=",
        "==",
        "===",
        "!",
        "\"",
        "\\",
        "# comment\n",
        " ",
        "\r\n",
        "$",
        "$name",
        "@json",
        "if",
        "then",
        "else",
        "end",
        "try",
        "catch",
        "def",
        "as",
        "let",
        "null",
        "1.e+",
        "é",
        "λ",
        "🦀",
        "\u{00A0}",
        "\u{2028}",
    ];
    let piece_count = u32::try_from(PIECES.len()).unwrap();
    let mut state = 0x5eed_1234_u32;

    for _ in 0..1_024 {
        let mut source = String::new();
        let pieces = 1 + next(&mut state) % 48;
        for _ in 0..pieces {
            let index = usize::try_from(next(&mut state) % piece_count).unwrap();
            source.push_str(PIECES[index]);
        }

        let tokens = Lexer::new(&source)
            .expect("generated source fits compact syntax spans")
            .collect::<Vec<_>>();
        assert_eq!(tokens.last().map(|token| token.kind), Some(TokenKind::Eof));
        let mut previous_end = 0;
        for token in &tokens {
            assert_span(&source, token.span);
            assert!(token.span.start() >= previous_end);
            previous_end = token.span.end();
        }

        // The trivia-skipping lexer is the parser-facing route; all three parse entry points see the corpus.
        let tokens = Lexer::new(&source)
            .expect("generated source fits compact syntax spans")
            .collect::<Vec<_>>();
        assert_eq!(tokens.last().map(|token| token.kind), Some(TokenKind::Eof));
        let mut previous_end = 0;
        for token in &tokens {
            assert_span(&source, token.span);
            assert!(token.span.start() >= previous_end);
            previous_end = token.span.end();
        }

        let query = parse_query(source_ref(), &source);
        if let Some(expression) = query.syntax() {
            assert_span(&source, expression.span());
        }
        assert_diagnostics(&source, query.diagnostics());

        let program = parse_program(source_ref(), &source);
        if let Some(unit) = program.syntax() {
            assert_span(&source, unit.span);
        }
        assert_diagnostics(&source, program.diagnostics());

        let library = parse_library(source_ref(), &source);
        if let Some(unit) = library.syntax() {
            assert_span(&source, unit.span);
        }
        assert_diagnostics(&source, library.diagnostics());
    }
}
