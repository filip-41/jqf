//! Binding-pattern grammar.
//!
//! Owns destructuring patterns (`$x`, `[$a, $b]`, `{$k: $v}`, `$a ?// $b`, and the engine-binding `~x`) plus the shared
//! member-list loop they use with the object-literal grammar in `grammar.rs`. Recovery synchronization follows the
//! enclosing-form law of `grammar.rs`; the pattern-specific laws (nested patterns reject `?//`, patterns reject a
//! trailing comma) are stated at [`Parser::parse_pattern`] and the shared list helper.

use alloc::{boxed::Box, vec::Vec};

use jqf_source::Span;

use crate::{
    ExpectedTokens, GrammarContext, ObjectKey, ObjectPatternMember, Pattern, PatternKind, SyntaxErrorKind, TokenKind,
};

use super::Parser;

impl Parser<'_> {
    /// The pattern grammar's entry point: an alternative chain over the pattern atoms, or a plain atom when no `?//`
    /// follows.
    ///
    /// `?//` is allowed only at the TOP level of a pattern: a nested pattern (an array/object member) rejects it with
    /// `UnexpectedToken`, because the alternative belongs to the enclosing binding, not the member.
    pub(super) fn parse_pattern(&mut self) -> Pattern {
        self.parse_pattern_with_alternatives(true)
    }

    fn parse_nested_pattern(&mut self) -> Pattern {
        self.parse_pattern_with_alternatives(false)
    }

    fn parse_pattern_with_alternatives(&mut self, allow_alternative: bool) -> Pattern {
        let left = self.parse_pattern_atom();
        if !self.at(TokenKind::DestructureAlt) {
            return left;
        }
        if !allow_alternative {
            self.record_error(SyntaxErrorKind::UnexpectedToken, self.peek().span);
            self.bump();
            let _ = self.parse_pattern_atom();
            return left;
        }
        // Charge/release: see [`Parser::open_nesting_checked`]. Each `?//` link is one tree level of this alternative,
        // not of later siblings.
        let outer = self.grammar_depth;
        if !self.open_nesting_checked() {
            return left;
        }
        let operator_span = self.bump().span;
        let right = self.parse_pattern();
        self.restore_nesting(outer);
        let span = left.span().merge(right.span());
        Pattern::new(PatternKind::Alternative(Box::new(left), Box::new(right)), span).with_operator(operator_span)
    }

    /// The pattern grammar's descent point, guarded like the expression grammar's: an array or object pattern recurses
    /// into its members through here, so `. as [[[[…` nests the parser exactly as `[[[[…` does.
    fn parse_pattern_atom(&mut self) -> Pattern {
        let Some(outer) = self.open_nesting() else {
            return Pattern::new(PatternKind::Error, self.missing_span());
        };
        let pattern = self.parse_pattern_atom_inner();
        self.restore_nesting(outer);
        pattern
    }

    fn parse_pattern_atom_inner(&mut self) -> Pattern {
        // A caller-owned terminator is never consumed here: the enclosing form resynchronizes on it, so a missing
        // pattern is a zero-width insertion in front of it.
        if super::grammar::is_expression_sync(self.peek().kind) {
            let missing = self.missing_span();
            self.record_error(
                SyntaxErrorKind::ExpectedToken {
                    expected: ExpectedTokens::new(&[TokenKind::Variable, TokenKind::LBracket, TokenKind::LBrace]),
                    context: GrammarContext::Pattern,
                },
                missing,
            );
            return Pattern::new(PatternKind::Error, missing);
        }
        let token = self.bump();
        match token.kind {
            TokenKind::Variable => {
                self.reject_reserved_location_binder(token.span);
                Pattern::new(PatternKind::Variable, token.span)
            }
            // The engine-binding pattern `~x`: the `~` marker introduces an ENGINE binding (an engine constructor's
            // cursor), lexically scoped exactly like `$x`. The value grammar rejects it at lower time.
            TokenKind::Tilde => {
                if !self.at(TokenKind::Ident) {
                    self.record_error(
                        SyntaxErrorKind::ExpectedToken {
                            expected: ExpectedTokens::new(&[TokenKind::Ident]),
                            context: GrammarContext::EngineSurface,
                        },
                        self.peek().span,
                    );
                    // The caller's terminator closes the enclosing form, so the marker alone spans this recovery.
                    let end = if super::grammar::is_expression_sync(self.peek().kind) {
                        token.span
                    } else {
                        self.bump().span
                    };
                    return Pattern::new(PatternKind::Error, token.span.merge(end));
                }
                let name = self.bump();
                Pattern::new(PatternKind::EngineBinding, token.span.merge(name.span))
            }
            TokenKind::LBracket => self.parse_array_pattern(token.span),
            TokenKind::LBrace => self.parse_object_pattern(token.span),
            _ => {
                self.record_error(
                    SyntaxErrorKind::ExpectedToken {
                        expected: ExpectedTokens::new(&[TokenKind::Variable, TokenKind::LBracket, TokenKind::LBrace]),
                        context: GrammarContext::Pattern,
                    },
                    token.span,
                );
                Pattern::new(PatternKind::Error, token.span)
            }
        }
    }

    fn parse_array_pattern(&mut self, open: Span) -> Pattern {
        if self.at(TokenKind::RBracket) {
            let close = self.bump().span;
            // Every delimiter diagnostic points a secondary label at the authored opener; the empty pattern is not
            // exempt.
            self.record_error_with_opener(SyntaxErrorKind::ExpectedExpression, close, open);
            return Pattern::new(PatternKind::Array(Vec::new()), open.merge(close)).with_delimiters(close);
        }
        let (items, close) = super::grammar::parse_delimited_members(
            self,
            open,
            TokenKind::RBracket,
            GrammarContext::Pattern,
            &[TokenKind::Comma, TokenKind::RBracket, TokenKind::RBrace],
            false,
            |parser: &mut Parser<'_>| parser.parse_nested_pattern(),
        );
        Pattern::new(PatternKind::Array(items), open.merge(close)).with_delimiters(close)
    }

    fn parse_object_pattern(&mut self, open: Span) -> Pattern {
        if self.at(TokenKind::RBrace) {
            let close = self.bump().span;
            // Every delimiter diagnostic points a secondary label at the authored opener; the empty pattern is not
            // exempt.
            self.record_error_with_opener(SyntaxErrorKind::ExpectedExpression, close, open);
            return Pattern::new(PatternKind::Object(Vec::new()), open.merge(close)).with_delimiters(close);
        }
        let (members, close) = super::grammar::parse_delimited_members(
            self,
            open,
            TokenKind::RBrace,
            GrammarContext::Pattern,
            &[TokenKind::Comma, TokenKind::RBracket, TokenKind::RBrace],
            false,
            |parser: &mut Parser<'_>| parser.parse_object_pattern_member(),
        );
        Pattern::new(PatternKind::Object(members), open.merge(close)).with_delimiters(close)
    }

    fn parse_object_pattern_member(&mut self) -> ObjectPatternMember {
        let key = if self.at(TokenKind::String) {
            let span = self.bump().span;
            ObjectKey::String(self.string_template(span))
        } else if self.at(TokenKind::Variable) {
            let span = self.bump().span;
            self.reject_reserved_location_binder(span);
            ObjectKey::Variable(span)
        } else if super::grammar::is_field_key(self.peek().kind) {
            let first = self.bump().span;
            ObjectKey::Name(self.finish_qualified_span(first))
        } else if self.at(TokenKind::Format) {
            ObjectKey::Expr(Box::new(self.parse_format_key(GrammarContext::Pattern)))
        } else if self.at(TokenKind::LParen) {
            // A parenthesized dynamic key reuses the group parser; the recovery context names the pattern so the
            // diagnostic reads correctly.
            let open = self.bump().span;
            let group = self.parse_group(open, GrammarContext::Pattern);
            ObjectKey::Expr(Box::new(group))
        } else {
            let missing = self.missing_span();
            self.record_error(
                SyntaxErrorKind::ExpectedToken {
                    expected: ExpectedTokens::new(&[
                        TokenKind::Ident,
                        TokenKind::String,
                        TokenKind::Variable,
                        TokenKind::Format,
                        TokenKind::LParen,
                    ]),
                    context: GrammarContext::Pattern,
                },
                missing,
            );
            self.synchronize(&[TokenKind::Comma, TokenKind::RBracket, TokenKind::RBrace]);
            ObjectKey::Name(missing)
        };
        let colon_span = self.at(TokenKind::Colon).then(|| self.bump().span);
        let pattern = if colon_span.is_some() {
            Some(self.parse_nested_pattern())
        } else {
            if !matches!(key, ObjectKey::Variable(_)) {
                self.record_error(SyntaxErrorKind::UnexpectedToken, key.span());
            }
            None
        };
        // The member spans its key onward: a malformed key recovers to a zero-width span, so a member never claims a
        // token it never consumed.
        let end = pattern.as_ref().map_or_else(|| key.span(), Pattern::span);
        let span = key.span().merge(end);
        ObjectPatternMember {
            key,
            pattern,
            colon_span,
            separator_span: None,
            span,
        }
    }
}
