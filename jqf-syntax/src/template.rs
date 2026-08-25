//! Source scanner for string-template boundaries, shared by the grammar's string/format parsing (`grammar.rs` calls
//! `parts`). The lexer runs its own string-end scan; this module is the template-layer half of the same walk and is
//! deliberately kept in one place so the interpolation boundary law lives once.

use jqf_source::Span;

use crate::{Lexer, TokenKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TemplatePart {
    Literal(Span),
    Expression {
        span: Span,
        introducer_span: Span,
        close_span: Span,
    },
}

pub(crate) fn parts(source: &str, token_span: Span) -> TemplateParts<'_> {
    let token = &source[token_span.range()];
    let absolute_start = token_span.start() as usize;
    let content_start = 1_usize.min(token.len());
    let content_end = token.len().saturating_sub(1).max(content_start);
    TemplateParts {
        token,
        absolute_start,
        content_start,
        content_end,
        literal_start: content_start,
        cursor: content_start,
        pending_expression: None,
        emitted_any: false,
        finished: false,
    }
}

pub(crate) struct TemplateParts<'source> {
    token: &'source str,
    absolute_start: usize,
    content_start: usize,
    content_end: usize,
    literal_start: usize,
    cursor: usize,
    pending_expression: Option<TemplatePart>,
    emitted_any: bool,
    finished: bool,
}

impl Iterator for TemplateParts<'_> {
    type Item = TemplatePart;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(expression) = self.pending_expression.take() {
            self.emitted_any = true;
            return Some(expression);
        }
        if self.finished {
            return None;
        }

        while self.cursor + 1 < self.content_end {
            if self.token.as_bytes()[self.cursor] != b'\\' || self.token.as_bytes()[self.cursor + 1] != b'(' {
                self.cursor += 1;
                continue;
            }
            let preceding = self.token.as_bytes()[self.content_start..self.cursor]
                .iter()
                .rev()
                .take_while(|byte| **byte == b'\\')
                .count();
            if !preceding.is_multiple_of(2) {
                self.cursor += 2;
                continue;
            }

            let literal_start = self.literal_start;
            let introducer_start = self.cursor;
            let expression = self.scan_expression();
            if literal_start < introducer_start {
                self.pending_expression = Some(expression);
                self.emitted_any = true;
                return Some(TemplatePart::Literal(Span::from_usize(
                    self.absolute_start + literal_start,
                    self.absolute_start + introducer_start,
                )));
            }
            self.emitted_any = true;
            return Some(expression);
        }

        self.finished = true;
        if self.literal_start < self.content_end || !self.emitted_any {
            self.emitted_any = true;
            return Some(TemplatePart::Literal(Span::from_usize(
                self.absolute_start + self.literal_start,
                self.absolute_start + self.content_end,
            )));
        }
        None
    }
}

impl TemplateParts<'_> {
    fn scan_expression(&mut self) -> TemplatePart {
        let expression_start = self.cursor + 2;
        let mut depth = 1_u32;
        let mut expression_end = self.content_end;
        let mut close_start = self.content_end;
        let mut close_end = self.content_end;
        for inner in Lexer::new_unchecked(&self.token[expression_start..self.content_end]) {
            match inner.kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        expression_end = expression_start + inner.span.start() as usize;
                        close_start = expression_end;
                        close_end = expression_start + inner.span.end() as usize;
                        self.cursor = close_end;
                        break;
                    }
                }
                TokenKind::Eof => break,
                _ => {}
            }
        }
        // Unreachable through the grammar: the lexer refuses an unterminated interpolation before any template walk, so
        // the scan above always finds its closing parenthesis. Kept as the defensive floor — if that guarantee ever
        // lapses, the scanner must still resume PAST the interpolation rather than re-reading its body as literal text.
        if depth != 0 {
            self.cursor = self.content_end;
        }
        self.literal_start = self.cursor;
        TemplatePart::Expression {
            span: Span::from_usize(
                self.absolute_start + expression_start,
                self.absolute_start + expression_end,
            ),
            introducer_span: Span::from_usize(
                self.absolute_start + expression_start - 2,
                self.absolute_start + expression_start,
            ),
            close_span: Span::from_usize(self.absolute_start + close_start, self.absolute_start + close_end),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use super::*;

    fn span(start: usize, end: usize) -> Span {
        Span::from_usize(start, end)
    }

    fn parts_of(text: &str) -> Vec<TemplatePart> {
        parts(text, span(0, text.len())).collect()
    }

    fn expression_span(part: TemplatePart) -> (Span, Span, Span) {
        match part {
            TemplatePart::Expression {
                span,
                introducer_span,
                close_span,
            } => (span, introducer_span, close_span),
            TemplatePart::Literal(_) => panic!("expected expression part"),
        }
    }

    fn literal_span(part: TemplatePart) -> Span {
        match part {
            TemplatePart::Literal(span) => span,
            TemplatePart::Expression { .. } => panic!("expected literal part"),
        }
    }

    #[test]
    fn pure_literal_emits_one_part_over_the_interior() {
        assert_eq!(parts_of(r#""abc""#), vec![TemplatePart::Literal(span(1, 4))]);
        assert_eq!(parts_of(r#""""#), vec![TemplatePart::Literal(span(1, 1))]);
    }

    #[test]
    fn expression_is_split_into_literal_and_three_spans() {
        let parts = parts_of(r#""a\(.x)b""#);
        assert_eq!(parts.len(), 3);
        assert_eq!(literal_span(parts[0]), span(1, 2));
        let (expression, introducer, close) = expression_span(parts[1]);
        assert_eq!(expression, span(4, 6));
        assert_eq!(introducer, span(2, 4));
        assert_eq!(close, span(6, 7));
        assert_eq!(literal_span(parts[2]), span(7, 8));
    }

    #[test]
    fn nested_parens_keep_the_expression_balanced() {
        let parts = parts_of(r#""x\(f(1;2))y""#);
        assert_eq!(parts.len(), 3);
        assert_eq!(literal_span(parts[0]), span(1, 2));
        let (expression, introducer, close) = expression_span(parts[1]);
        assert_eq!(expression, span(4, 10));
        assert_eq!(introducer, span(2, 4));
        assert_eq!(close, span(10, 11));
        assert_eq!(literal_span(parts[2]), span(11, 12));
    }

    #[test]
    fn trailing_expression_emits_no_extra_literal() {
        let parts = parts_of(r#""a\(.x)""#);
        assert_eq!(parts.len(), 2);
        assert_eq!(literal_span(parts[0]), span(1, 2));
        let (expression, introducer, close) = expression_span(parts[1]);
        assert_eq!(expression, span(4, 6));
        assert_eq!(introducer, span(2, 4));
        assert_eq!(close, span(6, 7));
    }

    #[test]
    fn escaped_interpolation_stays_inside_the_literal() {
        let parts = parts_of(r#""literal=\\(not)""#);
        assert_eq!(parts.len(), 1);
        assert_eq!(literal_span(parts[0]), span(1, 16));
    }

    #[test]
    fn unbalanced_expression_consumes_the_rest_of_the_token() {
        let parts = parts_of(r#""\(.x""#);
        assert_eq!(parts.len(), 1);
        let (expression, introducer, close) = expression_span(parts[0]);
        assert_eq!(expression, span(3, 5));
        assert_eq!(introducer, span(1, 3));
        assert_eq!(close, span(5, 5), "empty close span at end of token");
    }
}
