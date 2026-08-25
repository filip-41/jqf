//! Source positions and structured diagnostics.
//!
//! Byte spans, source identity, borrowed source bytes, diagnostic codes, labels, and severity. `no_std`, with `alloc`
//! for owned messages.
//!
//! # Positions
//!
//! [`Span`] is a half-open byte range. [`SourceRef`] says which source, [`ResolvedSource`] carries the bytes a caller
//! retained for it.
//!
//! # Diagnostics
//!
//! A [`Diagnostic`] is a [`Code`], a [`Severity`], a message, and ordered [`DiagnosticSource`] / [`Label`] values.
//! [`Namespace`] builds the codes.
//!
#![deny(missing_docs)]
#![forbid(unsafe_code)]
#![no_std]

extern crate alloc;

mod code;
mod diagnostic;
mod source;
mod span;

pub use crate::code::{Code, Namespace};
pub use crate::diagnostic::{Diagnostic, DiagnosticSource, Label, LabelStyle, Severity};
pub use crate::source::{ResolvedSource, SourceFileRange, SourceId, SourceKind, SourceRef};
pub use crate::span::{Span, SpanError};

/// Compiles the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
