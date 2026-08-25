//! A diagnostic: code, severity, message, sources, and labels.
//!
//! `new` / `with_*` use ordinary `String` / `Vec` (abort on OOM). `try_new` / `try_*` return `None` if a reservation
//! fails, so a tight path can keep going without a diagnostic. Allocator refusal is not exercised in this crate.

use crate::{Code, SourceRef, Span};
use alloc::string::String;
use alloc::vec::Vec;

/// Copy `text` into an owned `String`. Returns `None` if the allocator refuses. Every `try_*` constructor in this file
/// goes through here.
fn try_owned(text: &str) -> Option<String> {
    let mut owned = String::new();
    owned.try_reserve_exact(text.len()).ok()?;
    owned.push_str(text);
    Some(owned)
}

/// Push `item` onto `vec`. Returns `None` if the extra slot cannot be reserved.
fn try_push<T>(vec: &mut Vec<T>, item: T) -> Option<()> {
    vec.try_reserve(1).ok()?;
    vec.push(item);
    Some(())
}

/// How serious a diagnostic is: `Error < Warning < Info < Trace`.
///
/// Derived `Ord` follows that order, so `min` is the error side and `max` is the trace side. Not `#[non_exhaustive]`:
/// adding a variant is a breaking change.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Severity {
    /// The run failed at this site.
    Error,
    /// Something was wrong; the run can still succeed.
    Warning,
    /// Extra note.
    Info,
    /// Extra detail.
    Trace,
}

/// Whether this is the main span or extra context.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum LabelStyle {
    /// The main span.
    Primary,
    /// Extra context, maybe on another source.
    Secondary,
}

/// A message attached to one [`SourceRef`] and [`Span`].
///
/// Primary is the main span; secondary is extra context, maybe on another source. The message is owned so the
/// diagnostic can outlive the source text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Label {
    style: LabelStyle,
    source: SourceRef,
    span: Span,
    message: String,
}

impl Label {
    /// Create a primary label.
    #[must_use]
    pub fn primary(source: SourceRef, span: Span, message: impl Into<String>) -> Self {
        Self::new(LabelStyle::Primary, source, span, message)
    }

    /// Create a secondary label.
    #[must_use]
    pub fn secondary(source: SourceRef, span: Span, message: impl Into<String>) -> Self {
        Self::new(LabelStyle::Secondary, source, span, message)
    }

    /// Build a label. Shared by [`Self::primary`] and [`Self::secondary`].
    fn new(style: LabelStyle, source: SourceRef, span: Span, message: impl Into<String>) -> Self {
        Self {
            style,
            source,
            span,
            message: message.into(),
        }
    }

    /// Same as [`Self::primary`], but returns `None` if the message cannot be allocated.
    ///
    /// There is no `try_secondary`. That is deliberate: the fallible path is a primary record or nothing.
    #[must_use]
    pub fn try_primary(source: SourceRef, span: Span, message: &str) -> Option<Self> {
        Some(Self::new(LabelStyle::Primary, source, span, try_owned(message)?))
    }

    /// Primary (main span) or secondary (extra context).
    #[must_use]
    pub const fn style(&self) -> LabelStyle {
        self.style
    }

    /// Source identity.
    #[must_use]
    pub const fn source(&self) -> SourceRef {
        self.source
    }

    /// Source span.
    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }

    /// Label message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Name and base offset for one [`SourceRef`], kept so a diagnostic can render after the source bytes are gone.
///
/// Labels stay relative to the segment. Add [`Self::base_offset`] when you need an absolute position. This does not
/// keep source bytes — that is [`crate::ResolvedSource`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticSource {
    source: SourceRef,
    label: String,
    base_offset: u64,
}

impl DiagnosticSource {
    /// Create owned diagnostic source metadata.
    #[must_use]
    pub fn new(source: SourceRef, label: impl Into<String>, base_offset: u64) -> Self {
        Self {
            source,
            label: label.into(),
            base_offset,
        }
    }

    /// Same as [`Self::new`], but returns `None` if the label cannot be allocated.
    #[must_use]
    pub fn try_new(source: SourceRef, label: &str, base_offset: u64) -> Option<Self> {
        Some(Self::new(source, try_owned(label)?, base_offset))
    }

    /// Source identity.
    #[must_use]
    pub const fn source(&self) -> SourceRef {
        self.source
    }

    /// Human-facing source label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Original-source byte offset at the start of the source segment.
    #[must_use]
    pub const fn base_offset(&self) -> u64 {
        self.base_offset
    }
}

/// A diagnostic: code, severity, message, sources, and labels.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    code: Code,
    severity: Severity,
    message: String,
    sources: Vec<DiagnosticSource>,
    labels: Vec<Label>,
}

impl Diagnostic {
    /// Create a diagnostic.
    #[must_use]
    pub fn new(code: Code, severity: Severity, message: impl Into<String>) -> Self {
        Self {
            code,
            severity,
            message: message.into(),
            sources: Vec::new(),
            labels: Vec::new(),
        }
    }

    /// Same as [`Self::new`], but returns `None` if the message cannot be allocated.
    #[must_use]
    pub fn try_new(code: Code, severity: Severity, message: &str) -> Option<Self> {
        Some(Self::new(code, severity, try_owned(message)?))
    }

    /// Append source metadata, preserving call order.
    #[must_use]
    pub fn with_source(mut self, source: DiagnosticSource) -> Self {
        self.sources.push(source);
        self
    }

    /// Same as [`Self::with_source`], but `None` if the list cannot grow. A failed reservation returns `None` and
    /// consumes the diagnostic.
    #[must_use]
    pub fn try_with_source(mut self, source: DiagnosticSource) -> Option<Self> {
        try_push(&mut self.sources, source)?;
        Some(self)
    }

    /// Append a label, preserving call order.
    #[must_use]
    pub fn with_label(mut self, label: Label) -> Self {
        self.labels.push(label);
        self
    }

    /// Same as [`Self::with_label`], but `None` if the list cannot grow. A failed reservation returns `None` and
    /// consumes the diagnostic.
    #[must_use]
    pub fn try_with_label(mut self, label: Label) -> Option<Self> {
        try_push(&mut self.labels, label)?;
        Some(self)
    }

    /// Diagnostic code.
    #[must_use]
    pub const fn code(&self) -> Code {
        self.code
    }

    /// Diagnostic severity. See [`Severity`] for order (`min` is the error side).
    #[must_use]
    pub const fn severity(&self) -> Severity {
        self.severity
    }

    /// Top-level diagnostic message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Source records, in the order they were added.
    ///
    /// A renderer may use the first record that matches a [`SourceRef`].
    #[must_use]
    pub fn sources(&self) -> &[DiagnosticSource] {
        &self.sources
    }

    /// Labels, in the order they were added.
    #[must_use]
    pub fn labels(&self) -> &[Label] {
        &self.labels
    }
}
