//! Parser state, token navigation, and grammar entry points.
//!
//! This module owns the token cursor (lookahead, consumption, diagnostics, and nesting accounting) together with the
//! grammar entry points that consume it: query expressions ([`grammar`]), binding patterns ([`pattern`]), and
//! program/library source units ([`source`]). Cursor mechanics and grammar live in one struct so grammar code never
//! reaches through a second layer.

mod grammar;
mod pattern;
mod source;

use alloc::vec::Vec;

use jqf_source::{Diagnostic, SourceRef, Span};

use crate::{
    ExpectedTokens, GrammarContext, Lexer, MAX_SYNTAX_NESTING_DEPTH, Parse, ParsedSyntax, SyntaxErrorKind,
    SyntaxInputError, Token, TokenKind, input::validate_source_len,
};

const OUTER_RECOVERY_BOUNDARIES: &[TokenKind] = &[
    TokenKind::Comma,
    TokenKind::Semi,
    TokenKind::Colon,
    TokenKind::Pipe,
    TokenKind::RParen,
    TokenKind::RBracket,
    TokenKind::RBrace,
    TokenKind::As,
    TokenKind::Then,
    TokenKind::Elif,
    TokenKind::Else,
    TokenKind::End,
    TokenKind::Catch,
];

/// Stateful reader over parser-facing tokens.
///
/// A parser borrows query text, tracks the source identity used by diagnostics, and exposes the small token operations
/// grammar functions need: inspect the current token, consume it, or require a specific kind. The nesting ceiling is
/// enforced here, during the one parse.
pub(crate) struct Parser<'src> {
    source: SourceRef,
    text: &'src str,
    end: usize,
    lexer: Lexer<'src>,
    current: Token,
    /// One token of lookahead past [`Self::current`], used when a real keyword might start a qualified name (`if::foo`,
    /// `def if::foo`).
    next: Token,
    diagnostics: Vec<Diagnostic>,
    /// Levels of program structure open at this point of the parse: one per grammar form the descent is inside, plus
    /// one per link of the operator chains still being built around it (a chain builds one tree level per link, so its
    /// links are nesting even though the parser loops over them). A completed chain releases its links with its
    /// subtree, so the count tracks the depth of the tree at this point and not the program's shape.
    grammar_depth: u32,
    /// Whether [`MAX_SYNTAX_NESTING_DEPTH`] has been reached. That ends the parse: input is forced to end-of-input so
    /// every open grammar form unwinds without consuming more, and no further diagnostic is recorded, so the depth
    /// refusal stays the first one the caller reports.
    grammar_depth_exhausted: bool,
}

impl<'src> Parser<'src> {
    /// Creates parser state for one query source.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxInputError::SourceTooLarge`] before lexing when compact syntax spans cannot represent the
    /// supplied source.
    pub(crate) fn new(source: SourceRef, text: &'src str) -> Result<Self, SyntaxInputError> {
        validate_source_len(text.len())?;
        Ok(Self::from_lexer(source, text, text.len(), Lexer::new_unchecked(text)))
    }

    fn within(source: SourceRef, text: &'src str, span: Span) -> Self {
        Self::from_lexer(source, text, span.end() as usize, Lexer::within(text, span))
    }

    fn from_lexer(source: SourceRef, text: &'src str, end: usize, mut lexer: Lexer<'src>) -> Self {
        let current = lexer.next().unwrap_or_else(|| eof_token(end));
        let next = lexer.next().unwrap_or_else(|| eof_token(end));
        let mut parser = Self {
            source,
            text,
            end,
            lexer,
            current,
            next,
            diagnostics: Vec::new(),
            grammar_depth: 0,
            grammar_depth_exhausted: false,
        };
        parser.record_current_lexer_error();
        parser
    }

    /// Parses one nested source range with its own parser state.
    ///
    /// The nested parser inherits its nesting depth on the SAME call stack, so `"\("\("…` cannot escape the ceiling
    /// one nested parser at a time.
    fn nested(&self, span: Span) -> Self {
        let mut parser = Self::within(self.source, self.text, span);
        parser.restore_nesting(self.grammar_depth);
        parser
    }

    /// Opens one level of program nesting, returning the level to restore.
    ///
    /// `None` means the nesting ceiling is reached: the refusal is recorded, input is already at end, and the caller's
    /// only remaining job is to unwind.
    fn open_nesting(&mut self) -> Option<u32> {
        let outer = self.grammar_depth;
        self.open_nesting_checked().then_some(outer)
    }

    /// Opens one level of program nesting.
    ///
    /// Returns `false` once [`MAX_SYNTAX_NESTING_DEPTH`] is reached, having recorded the refusal and forced
    /// end-of-input; the grammar's only job from there is to unwind.
    ///
    /// The operator-chain and definition/binding spines call this per link. Those helpers loop rather than recurse, so
    /// the PARSER's stack stays flat — but every link adds a tree level that the lowering walk and the syntax tree's
    /// own `Drop` do recurse over. Every link is charged, the first one included, at the level the chain is being built
    /// at, and the whole chain's links are released once that chain is complete: they are levels of the subtree just
    /// built, never of the siblings that follow it.
    fn open_nesting_checked(&mut self) -> bool {
        if self.grammar_depth_exhausted {
            return false;
        }
        self.grammar_depth = self.grammar_depth.saturating_add(1);
        if self.grammar_depth <= MAX_SYNTAX_NESTING_DEPTH {
            return true;
        }
        self.record_error(SyntaxErrorKind::NestingTooDeep, self.current.span);
        self.end_parse_at_nesting_refusal();
        false
    }

    /// Restores the nesting level an enclosing grammar form was entered at, discarding the levels its subtree opened
    /// — including the chain links, which belong to the subtree that is now complete.
    fn restore_nesting(&mut self, depth: u32) {
        self.grammar_depth = depth;
    }

    /// Inspect the current token without consuming it.
    fn peek(&self) -> &Token {
        &self.current
    }

    /// Kind of the token after [`Self::current`], not consumed.
    fn peek_ahead(&self) -> TokenKind {
        self.next.kind
    }

    /// Whether the current token has `kind`.
    fn at(&self, kind: TokenKind) -> bool {
        self.current.kind == kind
    }

    fn at_any(&self, kinds: &[TokenKind]) -> bool {
        kinds.contains(&self.current.kind)
    }

    /// Consume the current token.
    ///
    /// Consuming end-of-input returns the EOF token and leaves the parser at end-of-input.
    fn bump(&mut self) -> Token {
        let token = self.current;
        if token.kind != TokenKind::Eof {
            self.advance();
        }
        token
    }

    /// Consume `kind` when it is current, otherwise record an expected-token error.
    ///
    /// Mismatches do not consume input, leaving recovery decisions to the grammar function that called this method.
    fn expect_in(&mut self, kind: TokenKind, context: GrammarContext) -> Option<Token> {
        if self.at(kind) {
            Some(self.bump())
        } else {
            self.record_error(
                SyntaxErrorKind::ExpectedToken {
                    expected: ExpectedTokens::new(&[kind]),
                    context,
                },
                self.current.span,
            );
            None
        }
    }

    fn expect_or_missing(&mut self, kind: TokenKind, context: GrammarContext) -> Token {
        self.expect_in(kind, context)
            .unwrap_or_else(|| self.missing_token(kind))
    }

    fn expect_closer_or_missing(&mut self, kind: TokenKind, context: GrammarContext, opener: Span) -> Token {
        if self.at(kind) {
            self.bump()
        } else {
            self.record_error_with_opener(
                SyntaxErrorKind::UnclosedDelimiter {
                    expected: kind,
                    context,
                },
                self.peek().span,
                opener,
            );
            self.missing_token(kind)
        }
    }

    fn missing_span(&self) -> Span {
        Span::new(self.current.span.start(), self.current.span.start())
    }

    fn missing_token(&self, kind: TokenKind) -> Token {
        Token::new(kind, self.missing_span())
    }

    /// Pushes a syntax diagnostic unless the nesting refusal already ended the parse — that refusal is the one
    /// suppression, and it exists so the depth error stays the first thing the caller reports.
    fn record_error(&mut self, kind: SyntaxErrorKind, span: Span) {
        if self.grammar_depth_exhausted {
            return;
        }
        self.diagnostics.push(kind.diagnostic(self.source, span));
    }

    fn record_error_with_opener(&mut self, kind: SyntaxErrorKind, span: Span, opener: Span) {
        if self.grammar_depth_exhausted {
            return;
        }
        self.diagnostics
            .push(kind.diagnostic_with_opener(self.source, span, opener));
    }

    fn synchronize(&mut self, sync: &[TokenKind]) {
        if self.at_any(sync) || self.at(TokenKind::Eof) {
            return;
        }
        self.bump();
        while !self.at_any(sync) && !self.at(TokenKind::Eof) {
            self.bump();
        }
    }

    fn diagnostic_count(&self) -> usize {
        self.diagnostics.len()
    }

    fn span_text(&self, span: Span) -> &str {
        self.text.get(span.range()).unwrap_or_default()
    }

    /// `$__loc__` names the source-location binding. It is a legal variable in expression position and as an
    /// object-constructor shorthand (`{$__loc__}`), but it cannot appear as a binder: a pattern, label, `break` target,
    /// definition parameter, or import alias.
    fn reject_reserved_location_binder(&mut self, span: Span) {
        if self.span_text(span) == "$__loc__" {
            self.record_error(SyntaxErrorKind::UnexpectedToken, span);
        }
    }

    fn finish_at_eof(&mut self, diagnostics_before: usize) {
        if self.at(TokenKind::Eof) {
            return;
        }
        if self.diagnostic_count() > diagnostics_before {
            self.synchronize(&[TokenKind::Eof]);
        } else {
            self.expect_in(TokenKind::Eof, GrammarContext::Expression);
        }
    }

    /// Recovers to an authored closer, reporting an unclosed delimiter unless the enclosing form already diagnosed the
    /// failure.
    fn recover_closer(
        &mut self,
        kind: TokenKind,
        context: GrammarContext,
        opener: Span,
        diagnostics_before: usize,
    ) -> Token {
        if self.at(kind) {
            return self.bump();
        }
        if self.at(TokenKind::Eof) {
            return self.expect_closer_or_missing(kind, context, opener);
        }

        let already_diagnosed = self.diagnostic_count() > diagnostics_before;
        if !already_diagnosed {
            self.record_error_with_opener(
                SyntaxErrorKind::UnclosedDelimiter {
                    expected: kind,
                    context,
                },
                self.peek().span,
                opener,
            );
        }
        // The closer is always one of the outer recovery boundaries, so this is exactly `synchronize`'s bump-and-scan
        // over them.
        self.synchronize(OUTER_RECOVERY_BOUNDARIES);
        if self.at(kind) {
            self.bump()
        } else {
            if already_diagnosed && self.at(TokenKind::Eof) {
                self.record_error_with_opener(
                    SyntaxErrorKind::UnclosedDelimiter {
                        expected: kind,
                        context,
                    },
                    self.missing_span(),
                    opener,
                );
            }
            self.missing_token(kind)
        }
    }

    fn advance(&mut self) {
        self.current = self.next;
        self.next = self.lexer.next().unwrap_or_else(|| eof_token(self.end));
        self.record_current_lexer_error();
    }

    fn record_current_lexer_error(&mut self) {
        if self.current.kind == TokenKind::Error {
            self.record_error(self.current_error_kind(), self.current.span);
        }
    }

    fn current_error_kind(&self) -> SyntaxErrorKind {
        let spelling = self.span_text(self.current.span);
        if spelling.starts_with('$') {
            SyntaxErrorKind::InvalidVariable
        } else if self.is_separated_accessor(spelling) {
            SyntaxErrorKind::SeparatedAccessor
        } else if spelling.starts_with('"') {
            SyntaxErrorKind::UnterminatedString
        } else {
            SyntaxErrorKind::InvalidToken
        }
    }

    fn is_separated_accessor(&self, spelling: &str) -> bool {
        matches!(spelling.as_bytes().first(), Some(b'@' | b'&'))
            && previous_non_whitespace_byte(self.text, self.current.span.range().start) == Some(b'.')
    }

    fn into_diagnostics(self) -> Vec<Diagnostic> {
        self.diagnostics
    }

    /// Appends a nested parse's diagnostics and inherits its nesting refusal.
    ///
    /// A nested parser runs on the same call stack and inherits this parser's depth, so its refusal is this parser's
    /// refusal, and the ceiling law needs both directions of that. Adopting the refusal stops this parse from recording
    /// after the ceiling was hit; dropping a later nested parse's diagnostics stops a SIBLING interpolation from
    /// appending past it. Either leak would cost the depth refusal its place as the first and only thing the caller
    /// reports.
    fn append_nested_diagnostics(&mut self, diagnostics: Vec<Diagnostic>, refused: bool) {
        if self.grammar_depth_exhausted {
            return;
        }
        self.diagnostics.extend(diagnostics);
        if refused {
            self.end_parse_at_nesting_refusal();
        }
    }

    /// Whether the nesting ceiling ended this parse.
    const fn nesting_refused(&self) -> bool {
        self.grammar_depth_exhausted
    }

    /// Ends the parse at the nesting ceiling: input is forced to end-of-input so every open grammar form unwinds
    /// without consuming more, and no further diagnostic is recorded.
    fn end_parse_at_nesting_refusal(&mut self) {
        self.grammar_depth_exhausted = true;
        let eof = eof_token(self.current.span.start() as usize);
        self.current = eof;
        self.next = eof;
    }

    fn finish_parse<T>(self, root: Option<T>) -> Parse<T> {
        let source = self.source;
        let source_len =
            u32::try_from(self.text.len()).expect("parser source admission guarantees compact span length");
        let syntax = root.map(|root| ParsedSyntax::new(source, source_len, root));
        Parse::new(syntax, self.into_diagnostics())
    }
}

fn eof_token(offset: usize) -> Token {
    Token::new(TokenKind::Eof, Span::from_usize(offset, offset))
}

fn previous_non_whitespace_byte(text: &str, before: usize) -> Option<u8> {
    text.as_bytes()
        .get(..before)?
        .iter()
        .rev()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
}
