//! Expression grammar.
//!
//! Owns the query-expression descent: prefix and primary forms, postfix chains (fields, indices, slices, `.@`/`.&`
//! accessors, optional `?`), calls, operators by precedence level, controls (`if`/`try`/`reduce`/
//! `foreach`/`label`/`break`), bindings (`as`, jqf `let`), definitions, and the engine-surface `~` forms. Recovery
//! synchronization is owned by the enclosing form: loops stop before their caller-owned separators and the caller
//! consumes them. Sibling parsers in `pattern.rs` (binding patterns) and `source.rs` (source units) share this module's
//! `Parser`.

use alloc::{boxed::Box, vec::Vec};

use jqf_source::Span;

use crate::ast::PostfixBuilder;
use crate::ast::{BinaryExpr, ConditionalBranch, TryExpr, UnaryExpr};
use crate::operator::OperatorPrecedence;
use crate::{
    AccessorSelector, AssignmentExpr, Associativity, BindingExpr, BindingForm, CallArgument, CallExpr, ConditionalExpr,
    DefinitionExpr, ExpectedTokens, Expr, ExprKind, FieldSelector, GrammarContext, InfixOperation, LoopExpr, ObjectKey,
    ObjectMember, ObjectPatternMember, OperatorSpec, Parse, Pattern, PostfixSegment, PostfixStep, StringTemplate,
    SyntaxErrorKind, TemplateSegment, TokenKind, UnaryOp,
};

use super::{OUTER_RECOVERY_BOUNDARIES, Parser};

impl Parser<'_> {
    /// Parse one query expression and require the input to end afterwards.
    #[must_use]
    pub(crate) fn parse_query(self) -> Parse<Expr> {
        self.parse_query_reporting_refusal().0
    }

    /// [`Self::parse_query`], also reporting whether the parse ended on the nesting refusal.
    ///
    /// A nested parse needs that answer: it shares the ceiling with its caller, so a refusal inside it has to end the
    /// caller's parse too.
    #[must_use]
    fn parse_query_reporting_refusal(mut self) -> (Parse<Expr>, bool) {
        let diagnostics_before = self.diagnostic_count();
        let syntax = if self.at(TokenKind::Eof) {
            self.record_error(SyntaxErrorKind::ExpectedExpression, self.peek().span);
            None
        } else {
            Some(self.parse_pipe())
        };
        self.finish_at_eof(diagnostics_before);
        let refused = self.nesting_refused();
        (self.finish_parse(syntax), refused)
    }

    pub(super) fn parse_pipe(&mut self) -> Expr {
        self.parse_operator_level(Self::parse_comma, OperatorPrecedence::Pipe)
    }

    /// An object member's value is a pipe over expression-level operands, never over the query-head forms (`label`,
    /// `def`, `as`): `{a: label $l0 | .}`, `{a: def f: 1; f}`, and `{a: . as $x | $x}` are rejected because the value
    /// is not a full query; only a grouped `(…)` reopens the whole query grammar. The same choice keeps the member
    /// separator out of the value: the comma is an operator of the `parse_comma` level, and the operand here starts
    /// below it at `parse_alternative`.
    fn parse_object_value(&mut self) -> Expr {
        self.parse_operator_level(Self::parse_alternative, OperatorPrecedence::Pipe)
    }

    fn parse_comma(&mut self) -> Expr {
        self.parse_operator_level(Self::parse_query_head, OperatorPrecedence::Choice)
    }

    fn parse_query_head(&mut self) -> Expr {
        if self.at(TokenKind::Def) {
            self.parse_definition_expr()
        } else if self.at(TokenKind::Label) {
            self.parse_label()
        } else {
            self.parse_binding()
        }
    }

    fn parse_definition_expr(&mut self) -> Expr {
        // Charge/release: see [`Self::open_nesting_checked`]. Each `def` on the spine is one tree level and does not
        // pass [`Self::parse_prefix`].
        let outer = self.grammar_depth;
        if !self.open_nesting_checked() {
            return Expr::new(ExprKind::Error, self.missing_span());
        }
        let definition = self.parse_def_item();
        let body = self.parse_pipe();
        self.restore_nesting(outer);
        let span = definition.span.merge(body.span());
        Expr::new(
            ExprKind::Definition(DefinitionExpr {
                definition: Box::new(definition),
                body: Box::new(body),
            }),
            span,
        )
    }

    fn parse_binding(&mut self) -> Expr {
        // `as`'s value takes alternative breadth (`$x = f // g` binds the whole alternative), while jqf `let`'s value
        // takes comma breadth below — the two binders deliberately differ.
        let value = self.parse_alternative();
        if !self.at(TokenKind::As) {
            return value;
        }
        // Charge/release: see [`Self::open_nesting_checked`]. Each `as` link is one tree level and does not pass
        // [`Self::parse_prefix`].
        let outer = self.grammar_depth;
        if !self.open_nesting_checked() {
            return value;
        }
        let as_keyword_span = self.bump().span;
        let pattern = self.parse_pattern();
        let pipe_span = self.expect_or_missing(TokenKind::Pipe, GrammarContext::Pattern).span;
        let body = self.parse_pipe();
        self.restore_nesting(outer);
        let span = value.span().merge(body.span());
        Expr::new(
            ExprKind::Binding(BindingExpr {
                form: BindingForm::As {
                    as_keyword_span,
                    pipe_span,
                },
                pattern,
                value: Box::new(value),
                body: Box::new(body),
            }),
            span,
        )
    }

    fn parse_alternative(&mut self) -> Expr {
        self.parse_operator_level(Self::parse_assignment, OperatorPrecedence::Alternative)
    }

    fn parse_assignment(&mut self) -> Expr {
        self.parse_operator_level(Self::parse_or, OperatorPrecedence::Assignment)
    }

    fn parse_or(&mut self) -> Expr {
        self.parse_operator_level(Self::parse_and, OperatorPrecedence::Or)
    }

    fn parse_and(&mut self) -> Expr {
        self.parse_operator_level(Self::parse_comparison, OperatorPrecedence::And)
    }

    fn parse_comparison(&mut self) -> Expr {
        self.parse_operator_level(Self::parse_additive, OperatorPrecedence::Comparison)
    }

    fn parse_additive(&mut self) -> Expr {
        self.parse_operator_level(Self::parse_multiplicative, OperatorPrecedence::Additive)
    }

    fn parse_multiplicative(&mut self) -> Expr {
        self.parse_operator_level(Self::parse_prefix, OperatorPrecedence::Multiplicative)
    }

    fn parse_operator_level<F>(&mut self, operand: F, precedence: OperatorPrecedence) -> Expr
    where
        F: Fn(&mut Self) -> Expr + Copy,
    {
        let first = operand(self);
        let Some(first_spec) = self.operator_at(precedence) else {
            return first;
        };
        // Charge/release: see [`Self::open_nesting_checked`]. Restore here so a completed chain does not hold its links
        // against later siblings.
        let outer = self.grammar_depth;
        let expression = match first_spec.associativity {
            Associativity::Left => self.parse_left_associative(first, operand, precedence),
            Associativity::Right => self.parse_right_associative(first, operand, precedence),
            Associativity::NonAssociative => self.parse_non_associative(first, operand, precedence),
        };
        self.restore_nesting(outer);
        expression
    }

    fn parse_left_associative<F>(&mut self, mut left: Expr, operand: F, precedence: OperatorPrecedence) -> Expr
    where
        F: Fn(&mut Self) -> Expr + Copy,
    {
        while let Some(spec) = self.operator_at(precedence) {
            debug_assert_eq!(spec.associativity, Associativity::Left);
            if !self.open_nesting_checked() {
                break;
            }
            let op_span = self.bump().span;
            let right = operand(self);
            left = make_infix(spec.operation, op_span, left, right);
        }
        left
    }

    fn parse_right_associative<F>(&mut self, first: Expr, operand: F, precedence: OperatorPrecedence) -> Expr
    where
        F: Fn(&mut Self) -> Expr + Copy,
    {
        let first_spec = self.operator_at(precedence).expect("operator checked by caller");
        // Charge the first link too; see [`Self::open_nesting_checked`].
        if !self.open_nesting_checked() {
            return first;
        }
        let first_op_span = self.bump().span;
        let second = operand(self);
        let Some(second_spec) = self.operator_at(precedence) else {
            return make_infix(first_spec.operation, first_op_span, first, second);
        };

        let mut pending = Vec::new();
        pending.push((first, first_spec.operation, first_op_span));
        let mut left = second;
        let mut operation = second_spec.operation;
        loop {
            if !self.open_nesting_checked() {
                break;
            }
            let op_span = self.bump().span;
            pending.push((left, operation, op_span));
            left = operand(self);
            let Some(next_spec) = self.operator_at(precedence) else {
                break;
            };
            debug_assert_eq!(next_spec.associativity, Associativity::Right);
            operation = next_spec.operation;
        }

        let mut right = left;
        while let Some((left, operation, op_span)) = pending.pop() {
            right = make_infix(operation, op_span, left, right);
        }
        right
    }

    fn parse_non_associative<F>(&mut self, left: Expr, operand: F, precedence: OperatorPrecedence) -> Expr
    where
        F: Fn(&mut Self) -> Expr + Copy,
    {
        let spec = self.operator_at(precedence).expect("operator checked by caller");
        // Charge/release: see [`Self::open_nesting_checked`].
        if !self.open_nesting_checked() {
            return left;
        }
        let op_span = self.bump().span;
        let right = operand(self);
        let expression = make_infix(spec.operation, op_span, left, right);
        if self.operator_at(precedence).is_some() {
            let kind = if precedence == OperatorPrecedence::Comparison {
                SyntaxErrorKind::ChainedComparison
            } else {
                SyntaxErrorKind::ChainedAssignment
            };
            self.record_error(kind, self.peek().span);
            self.bump();
            let _ = operand(self);
        }
        expression
    }

    fn operator_at(&self, precedence: OperatorPrecedence) -> Option<OperatorSpec> {
        OperatorSpec::for_token(self.peek().kind).filter(|spec| spec.precedence == precedence)
    }

    /// The grammar's main descent point into a nested expression.
    ///
    /// A group, an array or object constructor, a control form, a unary chain and an interpolation all reach their
    /// inner expression through here, so charging one level here is what bounds most of the recursive descent. Three
    /// entries do not pass through here: definitions and `as` bindings recurse into their own body and charge their
    /// level directly, as the comments at those two sites record, and an object-key group enters through
    /// [`Self::parse_object_member`] calling [`Self::parse_primary`] on the opening parenthesis. Every recursion cycle
    /// past any of those entries still passes through here exactly once, so depth is charged per level either way.
    fn parse_prefix(&mut self) -> Expr {
        let Some(outer) = self.open_nesting() else {
            return Expr::new(ExprKind::Error, self.missing_span());
        };
        let expr = self.parse_prefix_inner();
        self.restore_nesting(outer);
        expr
    }

    fn parse_prefix_inner(&mut self) -> Expr {
        // A real keyword followed by `::` is a qualified name (`if::foo`, `and::x`), not the control or operator form
        // the bare word starts. Kept in its own function so the Expr locals of that path do not sit in this frame:
        // nested groups recurse through here.
        if is_name_component(self.peek().kind) && self.peek_ahead() == TokenKind::DoubleColon {
            return self.parse_qualified_name_prefix();
        }
        let control = match self.peek().kind {
            TokenKind::If => Some(self.parse_if()),
            TokenKind::Try => Some(self.parse_try()),
            TokenKind::Reduce => Some(self.parse_loop(false)),
            TokenKind::Foreach => Some(self.parse_loop(true)),
            TokenKind::Break => Some(self.parse_break()),
            TokenKind::Let => Some(self.parse_let_or_name()),
            _ => None,
        };
        if let Some(control) = control {
            return self.parse_postfix_tail(control);
        }
        if self.at(TokenKind::Minus) {
            let op_span = self.bump().span;
            let expr = self.parse_prefix();
            let span = op_span.merge(expr.span());
            return Expr::new(
                ExprKind::Unary(UnaryExpr {
                    op: UnaryOp::Negate,
                    op_span,
                    expr: Box::new(expr),
                }),
                span,
            );
        }
        self.parse_postfix()
    }

    #[inline(never)]
    fn parse_qualified_name_prefix(&mut self) -> Expr {
        let first = self.bump().span;
        let name = self.parse_name_expr(first);
        self.parse_postfix_tail(name)
    }

    fn parse_postfix(&mut self) -> Expr {
        let base = self.parse_primary();
        self.parse_postfix_tail(base)
    }

    fn parse_postfix_tail(&mut self, mut base: Expr) -> Expr {
        loop {
            if self.at(TokenKind::LParen) && is_callable(&base) {
                if !self.open_nesting_checked() {
                    break;
                }
                base = self.parse_call(&base);
            } else if matches!(base.kind(), ExprKind::Format) && self.at(TokenKind::String) {
                if !self.open_nesting_checked() {
                    break;
                }
                base = self.parse_format_template(base);
            } else {
                break;
            }
        }

        let root_field_operator = self.root_field_operator_span(&base);
        let separated_identity =
            matches!(base.kind(), ExprKind::Identity) && self.is_separated_accessor_after_dot(base.span());
        let first = if let Some(operator_span) = root_field_operator {
            base = implied_identity(operator_span.start());
            Some(self.parse_field_postfix(operator_span))
        } else if separated_identity {
            return self.parse_separated_accessor(base.span(), base.span());
        } else {
            match self.parse_postfix_step(base.span()) {
                Ok(step) => step,
                Err(error) => return *error,
            }
        };
        let Some(first) = first else {
            return base;
        };

        let first_span = base.span().merge(first.span);
        let second = match self.parse_postfix_step(first_span) {
            Ok(step) => step,
            Err(error) => return *error,
        };
        let Some(second) = second else {
            return crate::PostfixExpr::finish_one(base, first);
        };
        // The first four steps are unrolled because chains are short in practice; a chain that reaches a fifth step
        // hands the rest to the builder loop.
        let second_span = first_span.merge(second.span);
        let third = match self.parse_postfix_step(second_span) {
            Ok(step) => step,
            Err(error) => return *error,
        };
        let Some(third) = third else {
            return crate::PostfixExpr::finish_two(base, [first, second]);
        };
        let third_span = second_span.merge(third.span);
        let fourth = match self.parse_postfix_step(third_span) {
            Ok(step) => step,
            Err(error) => return *error,
        };
        let Some(fourth) = fourth else {
            return crate::PostfixExpr::finish_three(base, [first, second, third]);
        };
        self.parse_postfix_multi(base, first, second, third, fourth)
    }

    #[inline(never)]
    fn parse_postfix_multi(
        &mut self,
        base: Expr,
        first: PostfixStep,
        second: PostfixStep,
        third: PostfixStep,
        fourth: PostfixStep,
    ) -> Expr {
        let mut postfix = PostfixBuilder::new(base, first, second);
        postfix.push(third);
        postfix.push(fourth);
        loop {
            let step = match self.parse_postfix_step(postfix.span()) {
                Ok(step) => step,
                Err(error) => return *error,
            };
            let Some(step) = step else {
                return postfix.finish();
            };
            postfix.push(step);
        }
    }

    fn parse_postfix_step(&mut self, span: Span) -> Result<Option<PostfixStep>, Box<Expr>> {
        if self.at(TokenKind::Question) {
            Ok(Some(self.parse_optional()))
        } else if self.at(TokenKind::LBracket) {
            Ok(Some(self.parse_bracket_postfix(None)))
        } else if self.at(TokenKind::Dot) {
            self.parse_dot_postfix(span).map(Some)
        } else if self.at(TokenKind::DotAt) {
            self.parse_accessor_postfix(span, Accessor::Node).map(Some)
        } else if self.at(TokenKind::DotAmp) {
            self.parse_accessor_postfix(span, Accessor::Attribute).map(Some)
        } else {
            Ok(None)
        }
    }

    fn root_field_operator_span(&self, base: &Expr) -> Option<Span> {
        if !matches!(base.kind(), ExprKind::Identity) {
            return None;
        }
        if !is_field_key(self.peek().kind) {
            return None;
        }
        // A `.field` postfix on identity is contiguous: the dot's end must touch the key's start (a quoted key may
        // follow whitespace because its token already spans the quote pair). The same contiguity law lives at
        // parse_dot_postfix; this is the root-form half of it.
        if self.peek().kind != TokenKind::String && base.span().end() != self.peek().span.start() {
            return None;
        }
        Some(base.span())
    }

    fn parse_dot_postfix(&mut self, base_span: Span) -> Result<PostfixStep, Box<Expr>> {
        let operator_span = self.bump().span;
        if is_field_key(self.peek().kind) {
            if self.peek().kind != TokenKind::String && operator_span.end() != self.peek().span.start() {
                self.record_error(SyntaxErrorKind::UnexpectedToken, self.peek().span);
            }
            Ok(self.parse_field_postfix(operator_span))
        } else if self.at(TokenKind::LBracket) {
            Ok(self.parse_bracket_postfix(Some(operator_span)))
        } else if self.is_separated_accessor_after_dot(operator_span) {
            Err(Box::new(self.parse_separated_accessor(base_span, operator_span)))
        } else {
            // The offending token stays with whatever form owns it: the node spans only the base and the dot this step
            // did consume.
            self.record_error(SyntaxErrorKind::ExpectedExpression, self.peek().span);
            Err(Box::new(Expr::new(ExprKind::Error, base_span.merge(operator_span))))
        }
    }

    fn is_separated_accessor_after_dot(&self, dot_span: Span) -> bool {
        dot_span.end() < self.peek().span.start()
            && ((self.at(TokenKind::Format) && self.span_text(self.peek().span).starts_with('@'))
                || (self.at(TokenKind::Error)
                    && matches!(self.span_text(self.peek().span).as_bytes().first(), Some(b'@' | b'&'))))
    }

    fn parse_separated_accessor(&mut self, base_span: Span, dot_span: Span) -> Expr {
        if !self.at(TokenKind::Error) {
            self.record_error(SyntaxErrorKind::SeparatedAccessor, dot_span.merge(self.peek().span));
        }
        let end = self.consume_separated_accessor();
        Expr::new(ExprKind::Error, base_span.merge(end))
    }

    fn consume_separated_accessor(&mut self) -> Span {
        let marker_is_error = self.at(TokenKind::Error);
        let marker = self.bump().span;
        if marker_is_error && is_field_key(self.peek().kind) {
            return self.bump().span;
        }
        marker
    }

    fn parse_field_postfix(&mut self, operator_span: Span) -> PostfixStep {
        let key = self.bump();
        let key_span = key.span;
        let selector = if key.kind == TokenKind::String {
            FieldSelector::String(self.string_template(key_span))
        } else {
            FieldSelector::Name(key_span)
        };
        self.finish_postfix(PostfixSegment::Field { selector }, operator_span, key_span)
    }

    fn parse_bracket_postfix(&mut self, operator_span: Option<Span>) -> PostfixStep {
        let open = self.bump();
        let diagnostics_before = self.diagnostic_count();
        if self.at(TokenKind::RBracket) {
            let close = self.bump();
            return self.finish_postfix(
                PostfixSegment::Index {
                    index: None,
                    open_span: open.span,
                    close_span: close.span,
                },
                operator_span.unwrap_or(open.span),
                close.span,
            );
        }

        let first = if self.at(TokenKind::Colon) {
            None
        } else {
            Some(self.parse_pipe())
        };
        if self.at(TokenKind::Colon) {
            let colon_span = self.bump().span;
            if operator_span.is_some() {
                // A dot does not introduce a slice: `expr.[a:b]` is a compile error. `Some` is passed only by the
                // dot-postfix arm, so a slice colon after a dot is rejected while the plain term postfix (`.[1:]`,
                // `[0][1:2]`, `5.[1:2]`) stays legal.
                self.record_error(SyntaxErrorKind::UnexpectedToken, colon_span);
            }
            let end = if self.at(TokenKind::RBracket) {
                None
            } else {
                Some(self.parse_pipe())
            };
            if first.is_none() && end.is_none() {
                // `.[:]` — BOTH bounds absent — is a SYNTAX error, not an engine rejection: the grammar requires at
                // least one bound and reports `unexpected ']'` here. `.[null:null]` remains the legal both-open
                // spelling, and `.[:b]`/`.[a:]` keep one open end.
                let missing = self.peek().span;
                self.record_error(SyntaxErrorKind::ExpectedExpression, missing);
            }
            let close = self.recover_closer(
                TokenKind::RBracket,
                GrammarContext::Index,
                open.span,
                diagnostics_before,
            );
            return self.finish_postfix(
                PostfixSegment::Slice {
                    start: first.map(Box::new),
                    end: end.map(Box::new),
                    colon_span,
                    open_span: open.span,
                    close_span: close.span,
                },
                operator_span.unwrap_or(open.span),
                close.span,
            );
        }

        let close = self.recover_closer(
            TokenKind::RBracket,
            GrammarContext::Index,
            open.span,
            diagnostics_before,
        );
        self.finish_postfix(
            PostfixSegment::Index {
                index: first.map(Box::new),
                open_span: open.span,
                close_span: close.span,
            },
            operator_span.unwrap_or(open.span),
            close.span,
        )
    }

    fn parse_accessor_postfix(&mut self, base_span: Span, accessor: Accessor) -> Result<PostfixStep, Box<Expr>> {
        let operator_span = self.bump().span;
        self.parse_accessor_postfix_after_operator(base_span, accessor, operator_span)
    }

    fn parse_accessor_postfix_after_operator(
        &mut self,
        base_span: Span,
        accessor: Accessor,
        operator_span: Span,
    ) -> Result<PostfixStep, Box<Expr>> {
        let context = match accessor {
            Accessor::Node => GrammarContext::NodeAccessor,
            Accessor::Attribute => GrammarContext::AttributeAccessor,
        };
        let Some((selector, segment_end)) = self.parse_accessor_selector(context) else {
            return Err(Box::new(Expr::new(ExprKind::Error, base_span.merge(operator_span))));
        };
        let segment = match accessor {
            Accessor::Node => PostfixSegment::NodeAccessor { selector },
            Accessor::Attribute => PostfixSegment::Attribute { selector },
        };
        Ok(self.finish_postfix(segment, operator_span, segment_end))
    }

    fn parse_accessor_selector(&mut self, context: GrammarContext) -> Option<(AccessorSelector, Span)> {
        if is_name_component(self.peek().kind) {
            let selector = self.bump().span;
            return Some((AccessorSelector::Direct { selector }, selector));
        }
        if self.at(TokenKind::LBracket) {
            let open = self.bump().span;
            let diagnostics_before = self.diagnostic_count();
            let selector = if self.at(TokenKind::String) {
                let span = self.bump().span;
                let template = self.string_template(span);
                for segment in template.segments() {
                    if let TemplateSegment::Expression { span, .. } = segment {
                        self.record_error(SyntaxErrorKind::UnexpectedToken, *span);
                    }
                }
                span
            } else {
                let error_span = self.peek().span;
                self.record_error(
                    SyntaxErrorKind::ExpectedToken {
                        expected: ExpectedTokens::new(&[TokenKind::String]),
                        context,
                    },
                    error_span,
                );
                // Recovery may consume nothing — the offending token can be the caller's — so the selector records
                // a zero-width insertion at the failure point rather than a token it does not own.
                let missing = self.missing_span();
                self.synchronize(&[
                    TokenKind::Comma,
                    TokenKind::Semi,
                    TokenKind::RParen,
                    TokenKind::RBracket,
                    TokenKind::RBrace,
                    TokenKind::Else,
                    TokenKind::Elif,
                    TokenKind::End,
                    TokenKind::Catch,
                ]);
                missing
            };
            let close = self
                .recover_closer(TokenKind::RBracket, context, open, diagnostics_before)
                .span;
            return Some((
                AccessorSelector::Bracket {
                    selector,
                    open_span: open,
                    close_span: close,
                },
                close,
            ));
        }
        if self.at(TokenKind::LParen) {
            let open = self.bump().span;
            let diagnostics_before = self.diagnostic_count();
            let selector = self.parse_pipe();
            let close = self
                .recover_closer(TokenKind::RParen, context, open, diagnostics_before)
                .span;
            return Some((
                AccessorSelector::Dynamic {
                    selector: Box::new(selector),
                    open_span: open,
                    close_span: close,
                },
                close,
            ));
        }
        self.record_error(SyntaxErrorKind::ExpectedExpression, self.peek().span);
        None
    }

    fn finish_postfix(&mut self, segment: PostfixSegment, operator_span: Span, segment_end: Span) -> PostfixStep {
        let optional_span = self.consume_optional_suffix();
        let span = operator_span.merge(optional_span.unwrap_or(segment_end));
        PostfixStep {
            segment,
            operator_span,
            optional_suffix_span: optional_span,
            span,
        }
    }

    fn parse_optional(&mut self) -> PostfixStep {
        let optional_span = self.bump().span;
        PostfixStep {
            segment: PostfixSegment::ErrorSuppression,
            operator_span: optional_span,
            optional_suffix_span: None,
            span: optional_span,
        }
    }

    fn consume_optional_suffix(&mut self) -> Option<Span> {
        self.at(TokenKind::Question).then(|| self.bump().span)
    }

    fn parse_call(&mut self, callee: &Expr) -> Expr {
        const SYNC: &[TokenKind] = &[TokenKind::Semi, TokenKind::RParen];

        let open = self.bump().span;
        let mut args = Vec::new();
        if self.at(TokenKind::RParen) {
            self.record_error(SyntaxErrorKind::ExpectedCallArgument, self.peek().span);
        } else {
            loop {
                let parsed_argument = if can_start_expression(self.peek().kind) {
                    let diagnostics_before = self.diagnostic_count();
                    let expression = self.parse_pipe();
                    args.push(CallArgument {
                        expression,
                        separator_span: None,
                    });
                    if !self.at_any(SYNC) && !self.at(TokenKind::Eof) {
                        if self.diagnostic_count() == diagnostics_before {
                            self.record_error(
                                SyntaxErrorKind::ExpectedToken {
                                    expected: ExpectedTokens::new(&[TokenKind::Semi, TokenKind::RParen]),
                                    context: GrammarContext::Call,
                                },
                                self.peek().span,
                            );
                        }
                        self.synchronize(SYNC);
                    }
                    true
                } else {
                    self.record_error(SyntaxErrorKind::ExpectedCallArgument, self.missing_span());
                    self.synchronize(SYNC);
                    false
                };

                if !self.at(TokenKind::Semi) {
                    break;
                }
                // A separator belongs to the argument it follows. An argument that failed to parse pushed nothing, so
                // its separator has no owner: writing it into the preceding argument would give that argument a
                // separator the source never wrote after it.
                let separator = self.bump().span;
                if parsed_argument && let Some(argument) = args.last_mut() {
                    argument.separator_span = Some(separator);
                }
                if self.at(TokenKind::RParen) {
                    self.record_error(SyntaxErrorKind::ExpectedCallArgument, self.peek().span);
                    break;
                }
            }
        }
        let close = self.expect_closer_or_missing(TokenKind::RParen, GrammarContext::Call, open);
        let (name, tilde_span) = match callee.kind() {
            ExprKind::Call(bare_call) => (bare_call.name, None),
            ExprKind::EngineTerm { tilde_span, name } => (*name, Some(*tilde_span)),
            _ => unreachable!("only bare named calls accept argument parentheses"),
        };
        let span = tilde_span.unwrap_or(name).merge(close.span);
        let call = CallExpr {
            name,
            args,
            parentheses: Some(open.merge(close.span)),
            close_parenthesis_missing: close.span.is_empty(),
        };
        Expr::new(
            match tilde_span {
                Some(tilde_span) => ExprKind::EngineCall { tilde_span, call },
                None => ExprKind::Call(call),
            },
            span,
        )
    }

    fn parse_primary(&mut self) -> Expr {
        if is_expression_sync(self.peek().kind) {
            let missing = self.missing_span();
            self.record_error(SyntaxErrorKind::ExpectedExpression, missing);
            return Expr::new(ExprKind::Error, missing);
        }
        let token = self.bump();
        match token.kind {
            TokenKind::Dot => Expr::new(ExprKind::Identity, token.span),
            TokenKind::DotDot => Expr::new(ExprKind::RecursiveDescent, token.span),
            TokenKind::Empty => self.literal_like_primary(token.span, ExprKind::Empty),
            TokenKind::Null => self.literal_like_primary(token.span, ExprKind::Null),
            TokenKind::True => self.literal_like_primary(token.span, ExprKind::Bool(true)),
            TokenKind::False => self.literal_like_primary(token.span, ExprKind::Bool(false)),
            TokenKind::Number => Expr::new(ExprKind::Number, token.span),
            TokenKind::String => Expr::new(ExprKind::String(self.string_template(token.span)), token.span),
            TokenKind::Variable => Expr::new(ExprKind::Variable, token.span),
            TokenKind::Ident => self.parse_name_expr(token.span),
            // The engine-surface marker: `~name` parses to a bare ENGINE TERM (an engine-binding reference, or an
            // engine-constructor name about to be called). Lowering resolves the name against the engine scope and the
            // closed constructor list.
            TokenKind::Tilde => self.parse_engine_term(token.span),
            TokenKind::Format => Expr::new(ExprKind::Format, token.span),
            TokenKind::DotAt => self.parse_root_accessor(token.span, Accessor::Node),
            TokenKind::DotAmp => self.parse_root_accessor(token.span, Accessor::Attribute),
            TokenKind::LParen => self.parse_group(token.span, GrammarContext::Group),
            TokenKind::LBracket => self.parse_array(token.span),
            TokenKind::LBrace => self.parse_object(token.span),
            _ => {
                self.record_error(SyntaxErrorKind::ExpectedExpression, token.span);
                Expr::new(ExprKind::Error, token.span)
            }
        }
    }

    fn parse_root_accessor(&mut self, operator_span: Span, accessor: Accessor) -> Expr {
        let base = implied_identity(operator_span.start());
        let step = match self.parse_accessor_postfix_after_operator(base.span(), accessor, operator_span) {
            Ok(step) => step,
            Err(error) => return *error,
        };
        crate::PostfixExpr::finish_one(base, step)
    }

    fn parse_name_expr(&mut self, first: Span) -> Expr {
        let name = self.finish_qualified_span(first);
        Expr::new(
            ExprKind::Call(CallExpr {
                name,
                args: Vec::new(),
                parentheses: None,
                close_parenthesis_missing: false,
            }),
            name,
        )
    }

    /// `empty` / `true` / `false` / `null` stay their primary forms unless the next token makes them a call
    /// (`empty(5)`, `true::name`).
    ///
    /// Not inlined: nested groups recurse through [`Self::parse_primary`], and the call-shaped `CallExpr` locals of
    /// this path must not enlarge that frame.
    #[inline(never)]
    fn literal_like_primary(&mut self, span: Span, bare: ExprKind) -> Expr {
        if self.at(TokenKind::LParen) || self.at(TokenKind::DoubleColon) {
            self.parse_name_expr(span)
        } else {
            Expr::new(bare, span)
        }
    }

    /// Parses the engine-surface term after the `~` marker: one identifier name. The term is bare (`~x`) until a
    /// following `(` turns it into an engine-constructor call (`~generator(...)`) in [`Self::parse_postfix_tail`].
    fn parse_engine_term(&mut self, tilde_span: Span) -> Expr {
        if !self.at(TokenKind::Ident) {
            self.record_error(
                SyntaxErrorKind::ExpectedToken {
                    expected: ExpectedTokens::new(&[TokenKind::Ident]),
                    context: GrammarContext::EngineSurface,
                },
                self.peek().span,
            );
            // Recovery stops before the caller's terminator: the `)` of `f(~)` closes the call, so this form neither
            // consumes it nor spans it.
            let end = if is_expression_sync(self.peek().kind) {
                tilde_span
            } else {
                self.bump().span
            };
            return Expr::new(ExprKind::Error, tilde_span.merge(end));
        }
        let name = self.bump();
        let span = tilde_span.merge(name.span);
        Expr::new(
            ExprKind::EngineTerm {
                tilde_span,
                name: name.span,
            },
            span,
        )
    }

    pub(super) fn parse_format_template(&mut self, format: Expr) -> Expr {
        let template_span = self.bump().span;
        let span = format.span().merge(template_span);
        Expr::new(
            ExprKind::FormatTemplate {
                format: Box::new(format),
                template: self.string_template(template_span),
            },
            span,
        )
    }

    pub(super) fn string_template(&mut self, token_span: Span) -> StringTemplate {
        let mut template = StringTemplate::empty(token_span);
        for raw in crate::template::parts(self.text, token_span) {
            match raw {
                crate::template::TemplatePart::Literal(span) => {
                    for (kind, error_span) in crate::string_decode::literal_errors(&self.text[span.range()], span) {
                        self.record_error(kind, error_span);
                    }
                    template.push(TemplateSegment::Literal { span });
                }
                crate::template::TemplatePart::Expression {
                    span,
                    introducer_span,
                    close_span,
                } => {
                    let (nested, refused) = self.nested(span).parse_query_reporting_refusal();
                    let diagnostics = nested.diagnostics().to_vec();
                    self.append_nested_diagnostics(diagnostics, refused);
                    let expression = nested
                        .syntax()
                        .map_or_else(|| Expr::new(ExprKind::Error, span), |syntax| syntax.clone().into_root());
                    template.push(TemplateSegment::Expression {
                        span,
                        expression: Box::new(expression),
                        introducer_span,
                        close_span,
                    });
                }
            }
        }
        template
    }

    fn parse_if(&mut self) -> Expr {
        let start = self.bump().span;
        let mut branches = Vec::new();
        branches.push(self.parse_conditional_branch(start));
        while self.at(TokenKind::Elif) {
            let keyword_span = self.bump().span;
            branches.push(self.parse_conditional_branch(keyword_span));
        }
        let (else_keyword_span, else_branch) = if self.at(TokenKind::Else) {
            let keyword_span = self.bump().span;
            let diagnostics_before = self.diagnostic_count();
            let branch = self.parse_pipe();
            self.recover_control_branch(diagnostics_before, OUTER_RECOVERY_BOUNDARIES);
            (Some(keyword_span), Some(Box::new(branch)))
        } else {
            (None, None)
        };
        let end = if self.at(TokenKind::End) {
            self.bump().span
        } else {
            self.record_error_with_opener(
                SyntaxErrorKind::UnterminatedControl {
                    context: GrammarContext::Conditional,
                },
                self.peek().span,
                start,
            );
            self.missing_span()
        };
        Expr::new(
            ExprKind::If(ConditionalExpr {
                branches,
                else_branch,
                else_keyword_span,
                end_keyword_span: end,
            }),
            start.merge(end),
        )
    }

    fn parse_conditional_branch(&mut self, keyword_span: Span) -> ConditionalBranch {
        // The shared outer boundary set, not a control-keyword-only set: a sync that stops only at control keywords
        // would consume the caller-owned closer (`,` then `)` in `(if , )`) and report it missing at EOF. Sibling forms
        // (fold slots, call arguments) already include their structural closers.
        let condition_diagnostics = self.diagnostic_count();
        let condition = self.parse_pipe();
        if !self.at_any(OUTER_RECOVERY_BOUNDARIES) && !self.at(TokenKind::Eof) {
            if self.diagnostic_count() == condition_diagnostics {
                self.record_error(
                    SyntaxErrorKind::ExpectedToken {
                        expected: ExpectedTokens::new(&[TokenKind::Then]),
                        context: GrammarContext::Conditional,
                    },
                    self.missing_span(),
                );
            }
            self.synchronize(OUTER_RECOVERY_BOUNDARIES);
        }
        let then_keyword_span = self
            .expect_or_missing(TokenKind::Then, GrammarContext::Conditional)
            .span;
        let branch_diagnostics = self.diagnostic_count();
        let then_branch = self.parse_pipe();
        self.recover_control_branch(branch_diagnostics, OUTER_RECOVERY_BOUNDARIES);
        ConditionalBranch {
            keyword_span,
            condition,
            then_keyword_span,
            then_branch,
        }
    }

    fn recover_control_branch(&mut self, diagnostics_before: usize, sync: &[TokenKind]) {
        if self.at_any(sync) || self.at(TokenKind::Eof) {
            return;
        }
        if self.diagnostic_count() == diagnostics_before {
            self.record_error(
                SyntaxErrorKind::ExpectedToken {
                    expected: ExpectedTokens::new(&[TokenKind::Elif, TokenKind::Else, TokenKind::End]),
                    context: GrammarContext::Conditional,
                },
                self.missing_span(),
            );
        }
        self.synchronize(sync);
    }

    fn parse_try(&mut self) -> Expr {
        let start = self.bump().span;
        let protected_diagnostics = self.diagnostic_count();
        let expr = self.parse_prefix();
        if self.diagnostic_count() > protected_diagnostics
            && !self.at_any(OUTER_RECOVERY_BOUNDARIES)
            && !self.at(TokenKind::Eof)
        {
            // The outer boundary set, for the same reason as the conditional's syncs: a control-only set would eat the
            // caller-owned closer.
            self.synchronize(OUTER_RECOVERY_BOUNDARIES);
        }
        let (catch_keyword_span, handler) = if self.at(TokenKind::Catch) {
            if let Some(error_span) = unparenthesized_try_operand_span(&expr) {
                self.record_error(SyntaxErrorKind::UnexpectedToken, error_span);
            }
            let catch_keyword_span = self.bump().span;
            if can_start_expression(self.peek().kind) {
                let handler_diagnostics = self.diagnostic_count();
                // The catch handler binds at the same term-level production as the body (`parse_prefix`), not at
                // operator breadth: in `try B catch H`, `H` is one term and any trailing operator composes on the whole
                // `try … catch …` OUTSIDE it. So `5 | try . catch . + 1` is `(try . catch .) + 1` → `6`, and
                // `false | try . catch false // 9` is `(try . catch false) // 9` → `9` (the term-level binding law).
                let handler = self.parse_prefix();
                if self.diagnostic_count() > handler_diagnostics
                    && !self.at_any(OUTER_RECOVERY_BOUNDARIES)
                    && !self.at(TokenKind::Eof)
                {
                    self.synchronize(OUTER_RECOVERY_BOUNDARIES);
                }
                (Some(catch_keyword_span), Some(Box::new(handler)))
            } else {
                self.record_error_with_opener(
                    SyntaxErrorKind::UnterminatedControl {
                        context: GrammarContext::Try,
                    },
                    self.peek().span,
                    catch_keyword_span,
                );
                let missing = self.missing_span();
                (
                    Some(catch_keyword_span),
                    Some(Box::new(Expr::new(ExprKind::Error, missing))),
                )
            }
        } else {
            (None, None)
        };
        let end = handler.as_ref().map_or_else(|| expr.span(), |handler| handler.span());
        Expr::new(
            ExprKind::Try(TryExpr {
                try_keyword_span: start,
                expr: Box::new(expr),
                catch_keyword_span,
                handler,
            }),
            start.merge(end),
        )
    }

    fn parse_loop(&mut self, foreach: bool) -> Expr {
        let start = self.bump().span;
        let context = if foreach {
            GrammarContext::Foreach
        } else {
            GrammarContext::Reduce
        };
        let source = self.parse_alternative();
        let (as_keyword_span, binding) = if self.at(TokenKind::As) {
            let as_keyword_span = self.bump().span;
            (as_keyword_span, self.parse_pattern())
        } else {
            let missing = self.missing_span();
            self.record_error(
                SyntaxErrorKind::ExpectedToken {
                    expected: ExpectedTokens::new(&[TokenKind::As]),
                    context,
                },
                missing,
            );
            (missing, crate::Pattern::new(crate::PatternKind::Error, missing))
        };
        let open_span = self.expect_or_missing(TokenKind::LParen, context).span;
        let init = self.parse_fold_slot(context);
        let update_separator_span = self.expect_or_missing(TokenKind::Semi, context).span;
        let update = self.parse_fold_slot(context);
        let (extract_separator_span, extract) = if self.at(TokenKind::Semi) {
            let separator = self.bump().span;
            let expression = self.parse_fold_slot(context);
            if foreach {
                (Some(separator), Some(Box::new(expression)))
            } else {
                // `reduce` has no extract slot, so the tree holds neither the slot nor its separator: a separator whose
                // expression the tree dropped would report a child that is not there.
                self.record_error(SyntaxErrorKind::UnexpectedToken, separator);
                (None, None)
            }
        } else {
            (None, None)
        };
        let close = self.expect_closer_or_missing(TokenKind::RParen, context, open_span);
        let loop_expr = LoopExpr {
            keyword_span: start,
            source: Box::new(source),
            as_keyword_span,
            binding,
            open_span,
            init: Box::new(init),
            update_separator_span,
            update: Box::new(update),
            extract_separator_span,
            extract,
            close_span: close.span,
        };
        let kind = if foreach {
            ExprKind::Foreach(loop_expr)
        } else {
            ExprKind::Reduce(loop_expr)
        };
        Expr::new(kind, start.merge(close.span))
    }

    fn parse_fold_slot(&mut self, context: GrammarContext) -> Expr {
        const SYNC: &[TokenKind] = &[TokenKind::Semi, TokenKind::RParen];

        let diagnostics_before = self.diagnostic_count();
        let expression = self.parse_pipe();
        if !self.at_any(SYNC) && !self.at(TokenKind::Eof) {
            if self.diagnostic_count() == diagnostics_before {
                self.record_error(
                    SyntaxErrorKind::ExpectedToken {
                        expected: ExpectedTokens::new(&[TokenKind::Semi, TokenKind::RParen]),
                        context,
                    },
                    self.missing_span(),
                );
            }
            self.synchronize(SYNC);
        }
        expression
    }

    fn parse_label(&mut self) -> Expr {
        // Charge/release: see [`Self::open_nesting_checked`]. Each `label` on the spine is one tree level and does not
        // pass [`Self::parse_prefix`].
        let outer = self.grammar_depth;
        if !self.open_nesting_checked() {
            return Expr::new(ExprKind::Error, self.missing_span());
        }
        let start = self.bump().span;
        let label = self.expect_or_missing(TokenKind::Variable, GrammarContext::Label).span;
        self.reject_reserved_location_binder(label);
        let pipe_span = self.expect_or_missing(TokenKind::Pipe, GrammarContext::Label).span;
        let body = self.parse_pipe();
        self.restore_nesting(outer);
        let span = start.merge(body.span());
        Expr::new(
            ExprKind::Label {
                label_keyword_span: start,
                label,
                pipe_span,
                body: Box::new(body),
            },
            span,
        )
    }

    fn parse_break(&mut self) -> Expr {
        let start = self.bump().span;
        let label = if self.at(TokenKind::Variable) {
            let span = self.bump().span;
            self.reject_reserved_location_binder(span);
            span
        } else {
            let missing = self.missing_span();
            self.record_error(SyntaxErrorKind::MissingBreakLabel, missing);
            missing
        };
        Expr::new(
            ExprKind::Break {
                break_keyword_span: start,
                label,
            },
            start.merge(label),
        )
    }

    fn parse_let(&mut self) -> Expr {
        let start = self.bump().span;
        let pattern = self.parse_pattern();
        let equals_span = self.expect_or_missing(TokenKind::Assign, GrammarContext::Let).span;
        // `let`'s value takes comma breadth (see parse_binding for the contrast with `as`).
        let value = self.parse_comma();
        let pipe_span = self.expect_or_missing(TokenKind::Pipe, GrammarContext::Let).span;
        let body = self.parse_pipe();
        let span = start.merge(body.span());
        Expr::new(
            ExprKind::Binding(BindingExpr {
                form: BindingForm::Let {
                    let_keyword_span: start,
                    equals_span,
                    pipe_span,
                },
                pattern,
                value: Box::new(value),
                body: Box::new(body),
            }),
            span,
        )
    }

    /// `let` is contextual at the expression site too: a following pattern keeps the binder (`let PAT = SRC | BODY`),
    /// while `let(`, `let::name`, and a bare `let` are names — the last being the user's `let/0` after `def let:
    /// …`. The qualified-`::` form never reaches here (the prefix dispatch takes it first); this arm owns the call
    /// parenthesis and the bare spelling.
    ///
    /// Not inlined: nested groups recurse through [`Self::parse_prefix_inner`], and this path's call-shaped locals must
    /// not enlarge that frame.
    #[inline(never)]
    fn parse_let_or_name(&mut self) -> Expr {
        // Entered at the `let` token: the decision reads the token AFTER it.
        if !can_start_pattern(self.peek_ahead()) {
            let start = self.bump().span;
            return self.parse_name_expr(start);
        }
        self.parse_let()
    }

    /// Parses a parenthesized group with the grammar context its recovery diagnostics report (a plain group, or a
    /// pattern-member key).
    pub(super) fn parse_group(&mut self, open: Span, context: GrammarContext) -> Expr {
        let diagnostics_before = self.diagnostic_count();
        let expr = self.parse_pipe();
        let close = self.recover_closer(TokenKind::RParen, context, open, diagnostics_before);
        Expr::new(
            ExprKind::Group {
                expression: Box::new(expr),
                open_span: open,
                close_span: close.span,
            },
            open.merge(close.span),
        )
    }

    fn parse_array(&mut self, open: Span) -> Expr {
        if self.at(TokenKind::RBracket) {
            let close = self.bump();
            return Expr::new(
                ExprKind::Array {
                    expression: None,
                    open_span: open,
                    close_span: close.span,
                },
                open.merge(close.span),
            );
        }
        let diagnostics_before = self.diagnostic_count();
        let element = self.parse_pipe();
        let close = self.recover_closer(TokenKind::RBracket, GrammarContext::Array, open, diagnostics_before);
        Expr::new(
            ExprKind::Array {
                expression: Some(Box::new(element)),
                open_span: open,
                close_span: close.span,
            },
            open.merge(close.span),
        )
    }
}

/// One item a member-list can hold and how its separator span is recorded.
///
/// Implemented for object-literal members, pattern members, and patterns themselves so the shared list loop needs a
/// single type bound instead of a second closure parameter (whose type Rust cannot infer alongside the item closure's).
pub(super) trait SeparatorSettable {
    fn set_separator_span(&mut self, span: Span);
}

impl SeparatorSettable for ObjectMember {
    fn set_separator_span(&mut self, span: Span) {
        self.separator_span = Some(span);
    }
}

impl SeparatorSettable for ObjectPatternMember {
    fn set_separator_span(&mut self, span: Span) {
        self.separator_span = Some(span);
    }
}

impl SeparatorSettable for Pattern {
    fn set_separator_span(&mut self, span: Span) {
        self.set_trailing_separator(span);
    }
}

/// The one member-list loop behind object literals and array/object patterns: parse items, recover to the caller-owned
/// separators/closer, consume commas, and close on the closer.
///
/// The three list forms differ only in the item parser, the closer, the sync set, and the trailing-comma law: the
/// object literal accepts `{a:1,}` silently, the patterns reject `{$a,}` with `UnexpectedToken`. `open` labels an
/// unclosed-delimiter diagnostic.
pub(super) fn parse_delimited_members<'src, T: SeparatorSettable>(
    parser: &mut Parser<'src>,
    open: Span,
    closer: TokenKind,
    context: GrammarContext,
    sync: &'static [TokenKind],
    allow_trailing_comma: bool,
    mut parse_item: impl FnMut(&mut Parser<'src>) -> T,
) -> (Vec<T>, Span) {
    let mut items = Vec::new();
    loop {
        let diagnostics_before = parser.diagnostic_count();
        items.push(parse_item(parser));
        if !parser.at_any(sync) && !parser.at(TokenKind::Eof) {
            if parser.diagnostic_count() == diagnostics_before {
                parser.record_error(
                    SyntaxErrorKind::ExpectedToken {
                        expected: ExpectedTokens::new(&[TokenKind::Comma, closer]),
                        context,
                    },
                    parser.peek().span,
                );
            }
            parser.synchronize(sync);
        }
        if !parser.at(TokenKind::Comma) {
            break;
        }
        let separator_span = parser.bump().span;
        items
            .last_mut()
            .expect("a list comma follows an item")
            .set_separator_span(separator_span);
        if parser.at(closer) {
            if !allow_trailing_comma {
                parser.record_error(SyntaxErrorKind::UnexpectedToken, parser.peek().span);
            }
            break;
        }
    }
    let close = parser.expect_closer_or_missing(closer, context, open).span;
    (items, close)
}

impl Parser<'_> {
    fn parse_object(&mut self, open: Span) -> Expr {
        if self.at(TokenKind::RBrace) {
            let close = self.bump();
            return Expr::new(
                ExprKind::Object {
                    members: Vec::new(),
                    open_span: open,
                    close_span: close.span,
                },
                open.merge(close.span),
            );
        }
        let (members, close) = parse_delimited_members(
            self,
            open,
            TokenKind::RBrace,
            GrammarContext::Object,
            &[TokenKind::Comma, TokenKind::RBrace],
            true,
            |parser: &mut Parser<'_>| parser.parse_object_member(),
        );
        Expr::new(
            ExprKind::Object {
                members,
                open_span: open,
                close_span: close,
            },
            open.merge(close),
        )
    }

    fn parse_object_member(&mut self) -> ObjectMember {
        let key = if self.at(TokenKind::String) {
            let span = self.bump().span;
            ObjectKey::String(self.string_template(span))
        } else if self.at(TokenKind::Variable) {
            ObjectKey::Variable(self.bump().span)
        } else if is_field_key(self.peek().kind) {
            let first = self.bump().span;
            ObjectKey::Name(self.finish_qualified_span(first))
        } else if self.at(TokenKind::Format) {
            ObjectKey::Expr(Box::new(self.parse_format_key(GrammarContext::Object)))
        } else if self.at(TokenKind::LParen) {
            let group = self.parse_primary();
            ObjectKey::Expr(Box::new(group))
        } else {
            let missing = self.missing_span();
            self.record_error(SyntaxErrorKind::MalformedObjectKey, missing);
            self.synchronize(&[TokenKind::Comma, TokenKind::RBrace]);
            ObjectKey::Name(missing)
        };
        let colon_span = self.at(TokenKind::Colon).then(|| self.bump().span);
        let value = if colon_span.is_some() {
            Some(self.parse_object_value())
        } else {
            // `{@text "k"}` is object shorthand: the format produces the key and the value is an implied lookup of that
            // key. Other expression keys (`{(.key)}`) have no such spelling and still need an explicit value.
            if let ObjectKey::Expr(expr) = &key
                && !matches!(expr.kind(), ExprKind::FormatTemplate { .. })
            {
                self.record_error(SyntaxErrorKind::UnexpectedToken, key.span());
            }
            None
        };
        // The member spans its key onward: a malformed key recovers to a zero-width span, so a member never claims a
        // token it never consumed.
        let end = value.as_ref().map_or_else(|| key.span(), Expr::span);
        let span = key.span().merge(end);
        ObjectMember::new(key, value, colon_span, span)
    }

    pub(super) fn parse_alias_span(&mut self, context: GrammarContext) -> Span {
        if is_literal_like_name(self.peek().kind)
            || (is_name_component(self.peek().kind) && self.peek_ahead() == TokenKind::DoubleColon)
        {
            let first = self.bump().span;
            return self.finish_qualified_span(first);
        }
        if self.at(TokenKind::Variable) {
            let span = self.bump().span;
            self.reject_reserved_location_binder(span);
            return span;
        }
        let missing = self.missing_span();
        self.record_error(
            SyntaxErrorKind::ExpectedToken {
                expected: ExpectedTokens::new(&[TokenKind::Ident, TokenKind::Variable]),
                context,
            },
            missing,
        );
        if !is_expression_sync(self.peek().kind) && !self.at(TokenKind::Colon) {
            self.bump();
        }
        missing
    }

    pub(super) fn parse_name_span(&mut self, context: GrammarContext) -> Span {
        if is_literal_like_name(self.peek().kind)
            || (is_name_component(self.peek().kind) && self.peek_ahead() == TokenKind::DoubleColon)
        {
            let first = self.bump().span;
            return self.finish_qualified_span(first);
        }
        let missing = self.missing_span();
        self.record_error(
            SyntaxErrorKind::ExpectedToken {
                expected: ExpectedTokens::new(&[TokenKind::Ident]),
                context,
            },
            missing,
        );
        if !is_expression_sync(self.peek().kind) && !self.at(TokenKind::Colon) {
            self.bump();
        }
        missing
    }

    /// Parses an `@format "…"` object key into the format-template expression it spells.
    ///
    /// The format tag is a key only together with the string it formats, so a bare `@format` in key position is a
    /// missing-string error rather than a key of its own. The result takes the ordinary expression-key route, so no key
    /// form is added to [`ObjectKey`].
    pub(super) fn parse_format_key(&mut self, context: GrammarContext) -> Expr {
        let format = Expr::new(ExprKind::Format, self.bump().span);
        if self.at(TokenKind::String) {
            return self.parse_format_template(format);
        }
        self.record_error(
            SyntaxErrorKind::ExpectedToken {
                expected: ExpectedTokens::new(&[TokenKind::String]),
                context,
            },
            self.missing_span(),
        );
        format
    }

    pub(super) fn finish_qualified_span(&mut self, first: Span) -> Span {
        let mut span = first;
        while self.at(TokenKind::DoubleColon) {
            let separator = self.bump().span;
            let next = if is_name_component(self.peek().kind) {
                self.bump().span
            } else {
                self.record_error(
                    SyntaxErrorKind::ExpectedToken {
                        expected: ExpectedTokens::new(&[TokenKind::Ident]),
                        context: GrammarContext::Expression,
                    },
                    self.missing_span(),
                );
                self.missing_span()
            };
            span = span.merge(separator).merge(next);
        }
        span
    }
}

#[derive(Clone, Copy)]
enum Accessor {
    Node,
    Attribute,
}

/// The identity a root postfix form implies but never spells.
///
/// `.a`, `.@x` and `.&x` spend their dot on the step's operator, so the chain's base is implied: zero width at the dot,
/// owning no byte, which leaves the dot owned exactly once. `.` and `.[1]` are the other half of the law — there the
/// dot IS the identity and the step's operator is the bracket, as the `bracket_introducer` inventory records.
fn implied_identity(at: u32) -> Expr {
    Expr::new(ExprKind::Identity, Span::new(at, at))
}

fn make_infix(operation: InfixOperation, op_span: Span, left: Expr, right: Expr) -> Expr {
    let span = left.span().merge(right.span());
    match operation {
        InfixOperation::Binary(op) => Expr::new(
            ExprKind::Binary(BinaryExpr {
                op,
                op_span,
                left: Box::new(left),
                right: Box::new(right),
            }),
            span,
        ),
        InfixOperation::Assignment(op) => Expr::new(
            ExprKind::Assignment(AssignmentExpr {
                op,
                op_span,
                target: Box::new(left),
                value: Box::new(right),
            }),
            span,
        ),
    }
}

/// The keyword of a try operand that a `catch` cannot legally follow.
///
/// The operand comes from [`Parser::parse_prefix`], which parses one term: the operator levels, `as` bindings,
/// definitions and `label` all sit above it and cannot reach here. jqf `let` is the one binder
/// [`Parser::parse_prefix_inner`] admits, so it is the one form to reject.
fn unparenthesized_try_operand_span(expr: &Expr) -> Option<Span> {
    match expr.kind() {
        ExprKind::Binding(BindingExpr {
            form: BindingForm::Let { let_keyword_span, .. },
            ..
        }) => Some(*let_keyword_span),
        _ => None,
    }
}

fn is_callable(expr: &Expr) -> bool {
    matches!(
        expr.kind(),
        ExprKind::Call(CallExpr { parentheses: None, .. }) | ExprKind::EngineTerm { .. }
    )
}

pub(super) fn is_field_key(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident
            | TokenKind::String
            | TokenKind::And
            | TokenKind::Or
            | TokenKind::As
            | TokenKind::Def
            | TokenKind::Module
            | TokenKind::Import
            | TokenKind::Include
            | TokenKind::If
            | TokenKind::Then
            | TokenKind::Elif
            | TokenKind::Else
            | TokenKind::End
            | TokenKind::Try
            | TokenKind::Catch
            | TokenKind::Reduce
            | TokenKind::Foreach
            | TokenKind::Label
            | TokenKind::Break
            | TokenKind::Let
            | TokenKind::Empty
            | TokenKind::Null
            | TokenKind::True
            | TokenKind::False
    )
}

/// `let` joins the literal-like names as a CONTEXTUAL word: the binder keeps its token and expression role, while
/// definition, parameter, alias, and qualified-name positions accept the spelling — a contextual keyword stays a
/// legal name in every name position.
fn is_literal_like_name(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident | TokenKind::Empty | TokenKind::Null | TokenKind::True | TokenKind::False | TokenKind::Let
    )
}

fn is_name_component(kind: TokenKind) -> bool {
    is_field_key(kind) && kind != TokenKind::String
}

/// The tokens that can open a binding pattern (`$x`, `~x`, `[…`, `{…`), per the pattern grammar's atom set. After
/// `let`, one of these keeps the binder; anything else spells a name.
pub(super) fn can_start_pattern(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Variable | TokenKind::Tilde | TokenKind::LBracket | TokenKind::LBrace
    )
}

pub(super) fn can_start_expression(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Dot
            | TokenKind::DotDot
            | TokenKind::Empty
            | TokenKind::Null
            | TokenKind::True
            | TokenKind::False
            | TokenKind::Number
            | TokenKind::String
            | TokenKind::Variable
            | TokenKind::Ident
            | TokenKind::Format
            | TokenKind::DotAt
            | TokenKind::DotAmp
            | TokenKind::Tilde
            | TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::LBrace
            | TokenKind::If
            | TokenKind::Try
            | TokenKind::Reduce
            | TokenKind::Foreach
            | TokenKind::Label
            | TokenKind::Break
            | TokenKind::Let
            | TokenKind::Def
            | TokenKind::Minus
    )
}

pub(super) fn is_expression_sync(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Comma
            | TokenKind::Semi
            | TokenKind::RParen
            | TokenKind::RBracket
            | TokenKind::RBrace
            | TokenKind::Else
            | TokenKind::Elif
            | TokenKind::End
            | TokenKind::Catch
            | TokenKind::Eof
    )
}
