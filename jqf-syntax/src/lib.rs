//! The syntax model, lexer, parser, and grammar registry.
//!
//! This crate turns jqf query source into parser-facing syntax data while preserving byte spans back to the original
//! text. It describes source form only: name resolution, builtin validation, module loading, evaluation, and
//! presentation are outside this crate's API.
//!
//! The surface is [`parse_query`], [`parse_program`], [`parse_library`], and [`Lexer::new`]. They reject source longer
//! than [`MAX_SYNTAX_SOURCE_BYTES`] before lexing and refuse nesting past [`MAX_SYNTAX_NESTING_DEPTH`] during the
//! parse. Every parsed form preserves authored byte spans back to the source; the closed node inventory lives in
//! [`SyntaxNodeKind`].

#![deny(missing_docs)]
#![forbid(unsafe_code)]
#![no_std]

extern crate alloc;

mod ast;
mod diagnostic;
mod input;
mod inventory;
mod lexer;
mod operator;
mod parser;
mod source_binding;
mod string_decode;
mod template;
mod token;
mod traversal;

use crate::parser::Parser;

pub use crate::ast::{
    AccessorSelector, AssignmentExpr, AssignmentOp, BinaryOp, BindingExpr, BindingForm, CallArgument, CallExpr,
    ConditionalExpr, DefItem, DefParameter, DefinitionExpr, Expr, ExprKind, FieldSelector, ImportItem, IncludeItem,
    LoopExpr, ObjectKey, ObjectMember, ObjectPatternMember, Parse, ParsedSyntax, Pattern, PatternKind, PostfixExpr,
    PostfixSegment, PostfixStep, SourceItem, SourceUnit, StringTemplate, TemplateSegment, UnaryOp,
};
pub use crate::diagnostic::{ExpectedTokens, GrammarContext, SyntaxErrorKind};
pub use crate::input::{MAX_SYNTAX_NESTING_DEPTH, MAX_SYNTAX_SOURCE_BYTES, SyntaxInputError};
pub use crate::lexer::Lexer;
pub use crate::operator::{Associativity, InfixOperation, OperatorSpec};
pub use crate::source_binding::{BoundSyntax, SyntaxSource, SyntaxSourceError};
pub use crate::string_decode::{StringDecodeError, SyntaxViewError, decode_literal_into};
pub use crate::token::{Token, TokenKind};
pub use crate::traversal::{SyntaxNodeKind, SyntaxNodeRef, SyntaxWalk, WalkEvent};

/// Parses one jqf query.
///
/// # Errors
///
/// Returns [`SyntaxInputError::SourceTooLarge`] before lexing when compact syntax spans cannot represent the supplied
/// source.
pub fn parse_query(source: jqf_source::SourceRef, text: &str) -> Result<Parse<Expr>, SyntaxInputError> {
    Ok(Parser::new(source, text)?.parse_query())
}

/// Parses one jqf program source unit.
///
/// # Errors
///
/// Returns [`SyntaxInputError::SourceTooLarge`] before lexing when compact syntax spans cannot represent the supplied
/// source.
pub fn parse_program(source: jqf_source::SourceRef, text: &str) -> Result<Parse<SourceUnit>, SyntaxInputError> {
    Ok(Parser::new(source, text)?.parse_program())
}

/// Parses one jqf library source unit.
///
/// # Errors
///
/// Returns [`SyntaxInputError::SourceTooLarge`] before lexing when compact syntax spans cannot represent the supplied
/// source.
pub fn parse_library(source: jqf_source::SourceRef, text: &str) -> Result<Parse<SourceUnit>, SyntaxInputError> {
    Ok(Parser::new(source, text)?.parse_library())
}

/// Compiles the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
