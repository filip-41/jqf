//! Diagnostic constructors for syntax errors.
//!
//! This module assigns stable codes and source labels to lexer and parser failures. It does not render diagnostics or
//! resolve source text.

use alloc::{format, string::String};

use jqf_source::{Code, Diagnostic, Label, Namespace, Severity, SourceRef, Span};

use crate::TokenKind;

const SYNTAX: Namespace = Namespace::new("syntax");
const EXPECTED_TOKEN_CAPACITY: usize = 5;

/// Compact ordered set of tokens accepted at one grammar position.
///
/// The fixed five-token storage covers the parser's largest focused expectation without a heap allocation or one enum
/// variant per slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExpectedTokens {
    tokens: [TokenKind; EXPECTED_TOKEN_CAPACITY],
    len: u8,
}

impl ExpectedTokens {
    /// The tokens accepted at one grammar position, in diagnostic order.
    ///
    /// At most five tokens are retained; a longer list keeps its first slots rather than refusing, because a diagnostic
    /// is never worth ending a parse over.
    #[must_use]
    pub const fn new(tokens: &[TokenKind]) -> Self {
        // Slots past `len` are never read; `Eof` only fills the array.
        let mut slots = [TokenKind::Eof; EXPECTED_TOKEN_CAPACITY];
        let mut len: u8 = 0;
        while (len as usize) < tokens.len() && (len as usize) < EXPECTED_TOKEN_CAPACITY {
            slots[len as usize] = tokens[len as usize];
            len += 1;
        }
        Self { tokens: slots, len }
    }

    /// Required tokens in diagnostic order.
    #[must_use]
    pub fn as_slice(&self) -> &[TokenKind] {
        &self.tokens[..usize::from(self.len)]
    }
}

/// Focused grammar position attached to parser diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
#[non_exhaustive]
pub enum GrammarContext {
    /// General query expression.
    Expression,
    /// Parenthesized group.
    Group,
    /// Array constructor.
    Array,
    /// Object constructor or member.
    Object,
    /// Index or slice postfix.
    Index,
    /// Function call.
    Call,
    /// Node/value accessor selector.
    NodeAccessor,
    /// Markup-attribute accessor selector.
    AttributeAccessor,
    /// Binding pattern.
    Pattern,
    /// jqf `let` binding form.
    Let,
    /// Engine-surface term after the `~` marker.
    EngineSurface,
    /// Conditional control form.
    Conditional,
    /// `try` or authored `catch` control form.
    Try,
    /// `reduce` fold.
    Reduce,
    /// `foreach` fold.
    Foreach,
    /// Label control form.
    Label,
    /// Function definition.
    Definition,
    /// Module, import, include, or definition source item.
    SourceItem,
}

impl GrammarContext {
    const fn description(self) -> &'static str {
        match self {
            Self::Expression => "expression",
            Self::Group => "group",
            Self::Array => "array expression",
            Self::Object => "object expression",
            Self::Index => "index expression",
            Self::Call => "function call",
            Self::NodeAccessor => "node accessor",
            Self::AttributeAccessor => "attribute accessor",
            Self::Pattern => "binding pattern",
            Self::Let => "let expression",
            Self::EngineSurface => "engine-surface term",
            Self::Conditional => "conditional expression",
            Self::Try => "try expression",
            Self::Reduce => "reduce expression",
            Self::Foreach => "foreach expression",
            Self::Label => "label expression",
            Self::Definition => "function definition",
            Self::SourceItem => "source item",
        }
    }

    const fn opener_label(self) -> &'static str {
        match self {
            Self::Expression => "expression starts here",
            Self::Group => "group starts here",
            Self::Array => "array starts here",
            Self::Object => "object starts here",
            Self::Index => "index starts here",
            Self::Call => "call starts here",
            Self::NodeAccessor => "node accessor starts here",
            Self::AttributeAccessor => "attribute accessor starts here",
            Self::Pattern => "pattern starts here",
            Self::Let => "let expression starts here",
            Self::EngineSurface => "engine-surface term starts here",
            Self::Conditional => "conditional starts here",
            Self::Try => "catch starts here",
            Self::Reduce => "reduce expression starts here",
            Self::Foreach => "foreach expression starts here",
            Self::Label => "label expression starts here",
            Self::Definition => "definition starts here",
            Self::SourceItem => "source item starts here",
        }
    }
}

/// Error categories produced while reading jqf syntax.
///
/// Each category maps to a stable diagnostic code. Variants should describe the syntax failure, not a rendering
/// strategy or recovery action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SyntaxErrorKind {
    /// Source bytes did not form any token recognized by the lexer.
    InvalidToken,
    /// A variable spelling did not contain a valid variable name.
    InvalidVariable,
    /// A quoted string reached the end of input before its closing quote.
    UnterminatedString,
    /// A quoted string contains an unknown or incomplete escape.
    InvalidStringEscape,
    /// A Unicode escape contains an invalid UTF-16 surrogate sequence.
    InvalidUnicodeEscape,
    /// Whitespace separated `.` from a jqf accessor marker.
    SeparatedAccessor,
    /// The parser needed an expression at this source location.
    ExpectedExpression,
    /// A token appeared in a grammar position where it is not accepted.
    UnexpectedToken,
    /// A parenthesized call contained no filter argument.
    ExpectedCallArgument,
    /// A second unparenthesized comparison followed the first.
    ChainedComparison,
    /// A second unparenthesized assignment followed the first.
    ChainedAssignment,
    /// A delimited grammar form reached a boundary without its closer.
    UnclosedDelimiter {
        /// Closing token required by the grammar.
        expected: TokenKind,
        /// Delimited form that owns the closer.
        context: GrammarContext,
    },
    /// A control form reached its boundary without a required component.
    UnterminatedControl {
        /// Control form being parsed.
        context: GrammarContext,
    },
    /// An object member did not begin with a supported key form.
    MalformedObjectKey,
    /// `break` was not followed by a variable label.
    MissingBreakLabel,
    /// The parser needed one of a compact set of tokens at this location.
    ExpectedToken {
        /// Token kinds accepted by the grammar.
        expected: ExpectedTokens,
        /// Grammar position requiring the token.
        context: GrammarContext,
    },
    /// Program nesting reached [`crate::MAX_SYNTAX_NESTING_DEPTH`].
    NestingTooDeep,
}

impl SyntaxErrorKind {
    /// Stable diagnostic code for this error category.
    #[must_use]
    pub const fn code(self) -> Code {
        match self {
            Self::InvalidToken => SYNTAX.code("invalid-token"),
            Self::InvalidVariable => SYNTAX.code("invalid-variable"),
            Self::UnterminatedString => SYNTAX.code("unterminated-string"),
            Self::InvalidStringEscape => SYNTAX.code("invalid-string-escape"),
            Self::InvalidUnicodeEscape => SYNTAX.code("invalid-unicode-escape"),
            Self::SeparatedAccessor => SYNTAX.code("separated-accessor"),
            Self::ExpectedExpression => SYNTAX.code("expected-expression"),
            Self::UnexpectedToken => SYNTAX.code("unexpected-token"),
            Self::ExpectedCallArgument => SYNTAX.code("expected-call-argument"),
            Self::ChainedComparison => SYNTAX.code("chained-comparison"),
            Self::ChainedAssignment => SYNTAX.code("chained-assignment"),
            Self::UnclosedDelimiter { .. } => SYNTAX.code("unclosed-delimiter"),
            Self::UnterminatedControl { .. } => SYNTAX.code("unterminated-control"),
            Self::MalformedObjectKey => SYNTAX.code("malformed-object-key"),
            Self::MissingBreakLabel => SYNTAX.code("missing-break-label"),
            Self::ExpectedToken { .. } => SYNTAX.code("expected-token"),
            Self::NestingTooDeep => SYNTAX.code("nesting-too-deep"),
        }
    }

    /// Build a structured error diagnostic for `span` in `source`.
    #[must_use]
    pub fn diagnostic(self, source: SourceRef, span: Span) -> Diagnostic {
        Diagnostic::new(self.code(), Severity::Error, self.message()).with_label(Label::primary(
            source,
            span,
            self.label_message(),
        ))
    }

    /// Build a structured diagnostic with a secondary label at the authored opener.
    #[must_use]
    pub fn diagnostic_with_opener(self, source: SourceRef, span: Span, opener: Span) -> Diagnostic {
        let context = self.context();
        self.diagnostic(source, span)
            .with_label(Label::secondary(source, opener, context.opener_label()))
    }

    fn message(self) -> String {
        match self {
            Self::InvalidToken => "invalid token".into(),
            Self::InvalidVariable => "invalid variable".into(),
            Self::UnterminatedString => "unterminated string".into(),
            Self::InvalidStringEscape => "invalid string escape".into(),
            Self::InvalidUnicodeEscape => "invalid Unicode escape".into(),
            Self::SeparatedAccessor => "separated accessor introducer".into(),
            Self::ExpectedExpression => "expected expression".into(),
            Self::UnexpectedToken => "unexpected token".into(),
            Self::ExpectedCallArgument => "expected a filter argument in function call".into(),
            Self::ChainedComparison => "comparison chaining requires parentheses".into(),
            Self::ChainedAssignment => "assignment chaining requires parentheses".into(),
            Self::UnclosedDelimiter { expected, context } => format!(
                "unclosed {}; expected {}",
                context.description(),
                expected.description()
            ),
            Self::UnterminatedControl {
                context: GrammarContext::Conditional,
            } => "unterminated conditional expression; expected end keyword".into(),
            Self::UnterminatedControl {
                context: GrammarContext::Try,
            } => "unterminated catch; expected a handler expression".into(),
            Self::UnterminatedControl { context } => {
                format!("unterminated {}", context.description())
            }
            Self::MalformedObjectKey => "expected an object member key".into(),
            Self::MissingBreakLabel => "expected a variable label after break".into(),
            Self::ExpectedToken { expected, context } => {
                format!("expected {} in {}", format_expected(expected), context.description())
            }
            // The wording is the memory-governor's, deliberately: a program nested past the ceiling and a document
            // nested past it are the same refusal, and an operator reading one message should not have to learn a
            // second spelling for the other.
            Self::NestingTooDeep => format!(
                "nesting depth limit exceeded: the ceiling is {} levels",
                crate::MAX_SYNTAX_NESTING_DEPTH
            ),
        }
    }

    fn label_message(self) -> String {
        match self {
            Self::InvalidToken => "this token is not valid jqf syntax".into(),
            Self::InvalidVariable => "this variable needs a name".into(),
            Self::UnterminatedString => "this string needs a closing quote".into(),
            Self::InvalidStringEscape => "this escape is not valid in a jqf string".into(),
            Self::InvalidUnicodeEscape => "this Unicode escape is not a valid UTF-16 scalar sequence".into(),
            Self::SeparatedAccessor => "remove whitespace between `.` and the accessor marker".into(),
            Self::ExpectedExpression => "expected expression here".into(),
            Self::UnexpectedToken => "this token is not accepted here".into(),
            Self::ExpectedCallArgument => "empty parenthesized calls are invalid; use the bare name".into(),
            Self::ChainedComparison => "group one comparison before applying another".into(),
            Self::ChainedAssignment => "group one assignment before applying another".into(),
            Self::UnclosedDelimiter { expected, .. } => {
                format!("expected {} here", expected.description())
            }
            Self::UnterminatedControl {
                context: GrammarContext::Conditional,
            } => "expected end keyword here".into(),
            Self::UnterminatedControl {
                context: GrammarContext::Try,
            } => "expected a handler expression here".into(),
            Self::UnterminatedControl { .. } => "required control component is missing here".into(),
            Self::MalformedObjectKey => "expected a name, string, variable, or parenthesized key here".into(),
            Self::MissingBreakLabel => "expected a variable label here".into(),
            Self::ExpectedToken { expected, .. } => {
                format!("expected {} here", format_expected(expected))
            }
            Self::NestingTooDeep => "this is the level past the program nesting ceiling".into(),
        }
    }

    fn context(self) -> GrammarContext {
        match self {
            Self::UnclosedDelimiter { context, .. }
            | Self::UnterminatedControl { context }
            | Self::ExpectedToken { context, .. } => context,
            _ => GrammarContext::Expression,
        }
    }
}

fn format_expected(expected: ExpectedTokens) -> String {
    let slice = expected.as_slice();
    let mut message = String::new();
    for (index, token) in slice.iter().enumerate() {
        if index > 0 {
            if index + 1 == slice.len() {
                message.push_str(" or ");
            } else {
                message.push_str(", ");
            }
        }
        message.push_str(token.description());
    }
    message
}
