//! Public infix-operator syntax authority.
//!
//! One closed table ([`OperatorSpec::ALL`]) owns every infix operator's token spelling, precedence level, and
//! associativity; `for_token` resolves a token to its operator for the grammar. The grammar reads the table rather than
//! re-encoding precedence, so the table is the single authority.

use crate::{AssignmentOp, BinaryOp, TokenKind};

/// Authored grouping direction for operators at one precedence level.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Associativity {
    /// Repeated operators group from the left.
    Left,
    /// Repeated operators group from the right.
    Right,
    /// Repeated operators at this level require explicit grouping.
    NonAssociative,
}

/// Syntax-tree operation produced by one infix token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InfixOperation {
    /// Ordinary binary operation.
    Binary(BinaryOp),
    /// Assignment or update operation.
    Assignment(AssignmentOp),
}

/// Infix precedence levels from loosest to tightest binding.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum OperatorPrecedence {
    /// Filter composition.
    Pipe,
    /// Generator choice.
    Choice,
    /// Alternative/defaulting.
    Alternative,
    /// Assignment and update.
    Assignment,
    /// Logical disjunction.
    Or,
    /// Logical conjunction.
    And,
    /// Equality and ordering comparisons.
    Comparison,
    /// Addition and subtraction.
    Additive,
    /// Multiplication, division, and remainder.
    Multiplicative,
}

/// Complete syntax metadata for one infix token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperatorSpec {
    /// Source token.
    pub token: TokenKind,
    /// Syntax-tree operation.
    pub operation: InfixOperation,
    /// Binding strength.
    pub precedence: OperatorPrecedence,
    /// Grouping direction within the precedence level.
    pub associativity: Associativity,
}

impl OperatorSpec {
    /// Complete infix-operator inventory in precedence order.
    pub const ALL: &'static [Self] = &[
        Self::new(
            TokenKind::Pipe,
            InfixOperation::Binary(BinaryOp::Pipe),
            OperatorPrecedence::Pipe,
            Associativity::Right,
        ),
        Self::new(
            TokenKind::Comma,
            InfixOperation::Binary(BinaryOp::Comma),
            OperatorPrecedence::Choice,
            Associativity::Left,
        ),
        Self::new(
            TokenKind::Alt,
            InfixOperation::Binary(BinaryOp::Alternative),
            OperatorPrecedence::Alternative,
            Associativity::Right,
        ),
        Self::new(
            TokenKind::Assign,
            InfixOperation::Assignment(AssignmentOp::Assign),
            OperatorPrecedence::Assignment,
            Associativity::NonAssociative,
        ),
        Self::new(
            TokenKind::PipeAssign,
            InfixOperation::Assignment(AssignmentOp::Update),
            OperatorPrecedence::Assignment,
            Associativity::NonAssociative,
        ),
        Self::new(
            TokenKind::AddAssign,
            InfixOperation::Assignment(AssignmentOp::Add),
            OperatorPrecedence::Assignment,
            Associativity::NonAssociative,
        ),
        Self::new(
            TokenKind::SubAssign,
            InfixOperation::Assignment(AssignmentOp::Subtract),
            OperatorPrecedence::Assignment,
            Associativity::NonAssociative,
        ),
        Self::new(
            TokenKind::MulAssign,
            InfixOperation::Assignment(AssignmentOp::Multiply),
            OperatorPrecedence::Assignment,
            Associativity::NonAssociative,
        ),
        Self::new(
            TokenKind::DivAssign,
            InfixOperation::Assignment(AssignmentOp::Divide),
            OperatorPrecedence::Assignment,
            Associativity::NonAssociative,
        ),
        Self::new(
            TokenKind::ModAssign,
            InfixOperation::Assignment(AssignmentOp::Remainder),
            OperatorPrecedence::Assignment,
            Associativity::NonAssociative,
        ),
        Self::new(
            TokenKind::AltAssign,
            InfixOperation::Assignment(AssignmentOp::Alternative),
            OperatorPrecedence::Assignment,
            Associativity::NonAssociative,
        ),
        Self::new(
            TokenKind::Or,
            InfixOperation::Binary(BinaryOp::Or),
            OperatorPrecedence::Or,
            Associativity::Left,
        ),
        Self::new(
            TokenKind::And,
            InfixOperation::Binary(BinaryOp::And),
            OperatorPrecedence::And,
            Associativity::Left,
        ),
        Self::new(
            TokenKind::Eq,
            InfixOperation::Binary(BinaryOp::Equal),
            OperatorPrecedence::Comparison,
            Associativity::NonAssociative,
        ),
        Self::new(
            TokenKind::Ne,
            InfixOperation::Binary(BinaryOp::NotEqual),
            OperatorPrecedence::Comparison,
            Associativity::NonAssociative,
        ),
        Self::new(
            TokenKind::Lt,
            InfixOperation::Binary(BinaryOp::Less),
            OperatorPrecedence::Comparison,
            Associativity::NonAssociative,
        ),
        Self::new(
            TokenKind::Le,
            InfixOperation::Binary(BinaryOp::LessEqual),
            OperatorPrecedence::Comparison,
            Associativity::NonAssociative,
        ),
        Self::new(
            TokenKind::Gt,
            InfixOperation::Binary(BinaryOp::Greater),
            OperatorPrecedence::Comparison,
            Associativity::NonAssociative,
        ),
        Self::new(
            TokenKind::Ge,
            InfixOperation::Binary(BinaryOp::GreaterEqual),
            OperatorPrecedence::Comparison,
            Associativity::NonAssociative,
        ),
        Self::new(
            TokenKind::Plus,
            InfixOperation::Binary(BinaryOp::Add),
            OperatorPrecedence::Additive,
            Associativity::Left,
        ),
        Self::new(
            TokenKind::Minus,
            InfixOperation::Binary(BinaryOp::Subtract),
            OperatorPrecedence::Additive,
            Associativity::Left,
        ),
        Self::new(
            TokenKind::Star,
            InfixOperation::Binary(BinaryOp::Multiply),
            OperatorPrecedence::Multiplicative,
            Associativity::Left,
        ),
        Self::new(
            TokenKind::Slash,
            InfixOperation::Binary(BinaryOp::Divide),
            OperatorPrecedence::Multiplicative,
            Associativity::Left,
        ),
        Self::new(
            TokenKind::Percent,
            InfixOperation::Binary(BinaryOp::Remainder),
            OperatorPrecedence::Multiplicative,
            Associativity::Left,
        ),
    ];

    const fn new(
        token: TokenKind,
        operation: InfixOperation,
        precedence: OperatorPrecedence,
        associativity: Associativity,
    ) -> Self {
        Self {
            token,
            operation,
            precedence,
            associativity,
        }
    }

    /// Look up the syntax metadata for an infix token.
    ///
    /// A linear scan of [`Self::ALL`], which is the single authority for the operator table. The parser asks once per
    /// precedence level it descends through, so one operand costs one scan per level and a token that is not an
    /// operator at all walks the table to its end.
    #[must_use]
    pub fn for_token(token: TokenKind) -> Option<Self> {
        Self::ALL.iter().copied().find(|spec| spec.token == token)
    }
}
