//! The lexer: token kinds and byte spans.
//!
//! Run with `cargo run -p jqf-syntax --example lexer`.

use jqf_syntax::{Lexer, TokenKind};

fn main() {
    // `Lexer::new` skips whitespace and comments for parser consumers and emits one explicit end-of-input token. The
    // lexer classifies only; it does not decide whether a token is valid in its grammar position.
    let source = ".price.@tag // \"untagged\"";
    let tokens = Lexer::new(source)
        .expect("example source fits compact syntax spans")
        .collect::<Vec<_>>();
    let kinds: Vec<_> = tokens.iter().map(|token| token.kind).collect();
    assert_eq!(
        kinds,
        vec![
            TokenKind::Dot,
            TokenKind::Ident,
            TokenKind::DotAt,
            TokenKind::Ident,
            TokenKind::Alt,
            TokenKind::String,
            TokenKind::Eof,
        ]
    );

    // Every token carries a half-open byte span back into the source, so the exact spelling survives classification.
    let source = " .a";
    let tokens = Lexer::new(source)
        .expect("example source fits compact syntax spans")
        .collect::<Vec<_>>();
    assert_eq!(&source[tokens[0].span.range()], ".");
    assert_eq!(&source[tokens[1].span.range()], "a");
    assert_eq!(tokens[1].span.range(), 2..3);

    // Whitespace and comments are trivia: skipped, never emitted.
    let source = " # c\n. ";
    let kinds: Vec<_> = Lexer::new(source)
        .expect("example source fits compact syntax spans")
        .map(|token| token.kind)
        .collect();
    assert_eq!(kinds, vec![TokenKind::Dot, TokenKind::Eof]);

    println!("lexed {} tokens", kinds.len());
}
