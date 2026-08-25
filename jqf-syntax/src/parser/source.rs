//! Source-unit grammar: `def` items, imports, includes, module metadata, and the program/library units that hold them.
//!
//! Imports and includes retain their parsed path templates (including any interpolation) rather than reducing them to
//! opaque token spans. Recovery follows the enclosing-form law of `grammar.rs`.

use alloc::vec::Vec;

use crate::ast::ModuleItem;
use crate::{
    DefItem, DefParameter, ExpectedTokens, Expr, GrammarContext, ImportItem, IncludeItem, Parse, SourceItem,
    SourceUnit, SyntaxErrorKind, TokenKind,
};

use super::Parser;

impl Parser<'_> {
    /// Parse a program source unit that must end with a query expression.
    #[must_use]
    pub(crate) fn parse_program(mut self) -> Parse<SourceUnit> {
        let syntax = self.parse_source_unit(true);
        self.finish_parse(Some(syntax))
    }

    /// Parse a library source unit where the final query expression is optional.
    #[must_use]
    pub(crate) fn parse_library(mut self) -> Parse<SourceUnit> {
        let syntax = self.parse_source_unit(false);
        self.finish_parse(Some(syntax))
    }

    fn parse_source_unit(&mut self, require_expression: bool) -> SourceUnit {
        let start = self.peek().span;
        let mut items = Vec::new();
        if self.at(TokenKind::Module) {
            items.push(SourceItem::Module(self.parse_module_item()));
        }
        while self.at(TokenKind::Import) || self.at(TokenKind::Include) {
            if self.at(TokenKind::Import) {
                items.push(SourceItem::Import(self.parse_import_item()));
            } else {
                items.push(SourceItem::Include(self.parse_include_item()));
            }
        }
        while self.at(TokenKind::Def) {
            items.push(SourceItem::Def(self.parse_def_item()));
        }

        let expression_diagnostics_before = self.diagnostic_count();
        let expression = if self.at(TokenKind::Eof) {
            if require_expression {
                self.record_error(SyntaxErrorKind::ExpectedExpression, self.peek().span);
            }
            None
        } else {
            Some(self.parse_pipe())
        };
        self.finish_at_eof(expression_diagnostics_before);

        let end = expression
            .as_ref()
            .map_or_else(|| items.last().map_or(start, SourceItem::span), Expr::span);
        SourceUnit {
            items,
            expression,
            span: start.merge(end),
        }
    }

    fn parse_module_item(&mut self) -> ModuleItem {
        let start = self.bump().span;
        let diagnostics_before = self.diagnostic_count();
        let metadata = self.parse_pipe();
        let semi = self.finish_source_item(diagnostics_before);
        ModuleItem {
            module_keyword_span: start,
            span: start.merge(semi.span),
            metadata,
            semicolon_span: semi.span,
        }
    }

    fn parse_import_item(&mut self) -> ImportItem {
        let start = self.bump().span;
        let diagnostics_before = self.diagnostic_count();
        let path_span = self
            .expect_or_missing(TokenKind::String, GrammarContext::SourceItem)
            .span;
        let path = self.source_path_template(path_span);
        let as_keyword_span = self.expect_or_missing(TokenKind::As, GrammarContext::SourceItem).span;
        let alias = self.parse_alias_span(GrammarContext::SourceItem);
        let metadata = (!self.at(TokenKind::Semi)).then(|| self.parse_pipe());
        let semi = self.finish_source_item(diagnostics_before);
        ImportItem {
            import_keyword_span: start,
            path,
            as_keyword_span,
            alias,
            metadata,
            semicolon_span: semi.span,
            span: start.merge(semi.span),
        }
    }

    fn parse_include_item(&mut self) -> IncludeItem {
        let start = self.bump().span;
        let diagnostics_before = self.diagnostic_count();
        let path_span = self
            .expect_or_missing(TokenKind::String, GrammarContext::SourceItem)
            .span;
        let path = self.source_path_template(path_span);
        let metadata = (!self.at(TokenKind::Semi)).then(|| self.parse_pipe());
        let semi = self.finish_source_item(diagnostics_before);
        IncludeItem {
            include_keyword_span: start,
            path,
            metadata,
            semicolon_span: semi.span,
            span: start.merge(semi.span),
        }
    }

    pub(super) fn parse_def_item(&mut self) -> DefItem {
        let start = self.bump().span;
        let diagnostics_before = self.diagnostic_count();
        let name = self.parse_name_span(GrammarContext::Definition);
        let (params, parameter_parentheses, parameter_close_missing) = if self.at(TokenKind::LParen) {
            self.parse_def_params()
        } else {
            (Vec::new(), None, false)
        };
        let colon_span = self
            .expect_or_missing(TokenKind::Colon, GrammarContext::Definition)
            .span;
        let body = self.parse_pipe();
        let semi = self.finish_source_item(diagnostics_before);
        DefItem {
            def_keyword_span: start,
            name,
            params,
            parameter_parentheses,
            parameter_close_missing,
            colon_span,
            body,
            semicolon_span: semi.span,
            span: start.merge(semi.span),
        }
    }

    fn parse_def_params(&mut self) -> (Vec<DefParameter>, Option<jqf_source::Span>, bool) {
        let open = self.bump().span;
        let mut params = Vec::new();
        if self.at(TokenKind::RParen) {
            self.record_error(SyntaxErrorKind::ExpectedExpression, self.peek().span);
        } else {
            loop {
                let name = self.parse_alias_span(GrammarContext::Definition);
                if !self.at_any(&[TokenKind::Semi, TokenKind::RParen, TokenKind::Colon]) {
                    self.record_error(
                        SyntaxErrorKind::ExpectedToken {
                            expected: ExpectedTokens::new(&[TokenKind::Semi, TokenKind::RParen]),
                            context: GrammarContext::Definition,
                        },
                        self.peek().span,
                    );
                    self.synchronize(&[TokenKind::Semi, TokenKind::RParen, TokenKind::Colon]);
                }
                let separator_span = self.at(TokenKind::Semi).then(|| self.bump().span);
                params.push(DefParameter { name, separator_span });
                if separator_span.is_none() {
                    break;
                }
                if self.at(TokenKind::RParen) {
                    self.record_error(SyntaxErrorKind::ExpectedExpression, self.peek().span);
                    break;
                }
            }
        }
        let close = self
            .expect_closer_or_missing(TokenKind::RParen, GrammarContext::Definition, open)
            .span;
        (params, Some(open.merge(close)), close.is_empty())
    }

    fn source_path_template(&mut self, span: jqf_source::Span) -> crate::StringTemplate {
        if self.span_text(span).starts_with('"') {
            self.string_template(span)
        } else {
            crate::StringTemplate::empty(span)
        }
    }

    fn finish_source_item(&mut self, diagnostics_before: usize) -> crate::Token {
        const SYNC: &[TokenKind] = &[TokenKind::Semi, TokenKind::Eof];

        if !self.at(TokenKind::Semi) {
            if self.diagnostic_count() == diagnostics_before {
                self.record_error(
                    SyntaxErrorKind::ExpectedToken {
                        expected: ExpectedTokens::new(&[TokenKind::Semi]),
                        context: GrammarContext::SourceItem,
                    },
                    self.missing_span(),
                );
            }
            if !self.at(TokenKind::Eof) {
                self.synchronize(SYNC);
            }
        }
        if self.at(TokenKind::Semi) {
            self.bump()
        } else {
            self.missing_token(TokenKind::Semi)
        }
    }
}
