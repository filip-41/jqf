//! Checked binding of parsed syntax to retained source text.
//!
//! [`ParsedSyntax::bind`] verifies source identity, byte length, and UTF-8 alignment once, then [`BoundSyntax`] carries
//! the verified text so span consumption never re-checks them.

use core::fmt;

use jqf_source::{ResolvedSource, SourceRef};

use crate::ParsedSyntax;

/// UTF-8 source text verified against one parsed syntax artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyntaxSource<'source> {
    source: SourceRef,
    label: &'source str,
    text: &'source str,
}

impl<'source> SyntaxSource<'source> {
    /// Source identity verified while binding.
    #[must_use]
    pub const fn source_ref(&self) -> SourceRef {
        self.source
    }

    /// Human-facing source label supplied at binding.
    #[must_use]
    pub const fn label(&self) -> &str {
        self.label
    }

    /// UTF-8 source text verified while binding.
    #[must_use]
    pub const fn text(&self) -> &'source str {
        self.text
    }
}

/// Parsed root bound to checked retained source text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoundSyntax<'tree, 'source, T> {
    root: &'tree T,
    source: SyntaxSource<'source>,
}

impl<'tree, 'source, T> BoundSyntax<'tree, 'source, T> {
    /// Parsed root associated with the checked source text.
    #[must_use]
    pub const fn root(&self) -> &'tree T {
        self.root
    }

    /// Checked retained source text.
    #[must_use]
    pub const fn source(&self) -> &SyntaxSource<'source> {
        &self.source
    }
}

/// Failure to bind parsed syntax to retained source bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyntaxSourceError {
    /// The retained source identity does not match the syntax.
    SourceMismatch,
    /// The retained source byte length does not match the syntax.
    LengthMismatch,
    /// The retained bytes are not valid UTF-8.
    NonUtf8 {
        /// Byte offset of the first invalid sequence, so a caller can point at the offending byte instead of at the
        /// whole source.
        valid_up_to: usize,
    },
}

impl fmt::Display for SyntaxSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceMismatch => formatter.write_str("resolved source identity does not match syntax"),
            Self::LengthMismatch => formatter.write_str("resolved source byte length does not match syntax"),
            Self::NonUtf8 { valid_up_to } => {
                write!(formatter, "resolved source bytes are not UTF-8 from byte {valid_up_to}")
            }
        }
    }
}

impl core::error::Error for SyntaxSourceError {}

impl<T> ParsedSyntax<T> {
    /// Bind this tree to retained source bytes after identity, length, and UTF-8 checks.
    ///
    /// # Errors
    ///
    /// Returns a source-binding error when the retained bytes cannot prove the exact source used for parsing.
    pub fn bind<'tree, 'source>(
        &'tree self,
        resolved: ResolvedSource<'source>,
    ) -> Result<BoundSyntax<'tree, 'source, T>, SyntaxSourceError> {
        if resolved.source() != self.source_ref() {
            return Err(SyntaxSourceError::SourceMismatch);
        }
        // A source length the parser recorded as `u32` cannot exceed `usize` on any target this crate builds for, so
        // the conversion failing means the retained bytes cannot be the ones parsed — the same conclusion the length
        // comparison below reaches, reported the same way.
        let expected_len = usize::try_from(self.source_len()).map_err(|_| SyntaxSourceError::LengthMismatch)?;
        if resolved.bytes().len() != expected_len {
            return Err(SyntaxSourceError::LengthMismatch);
        }
        let text = core::str::from_utf8(resolved.bytes()).map_err(|error| SyntaxSourceError::NonUtf8 {
            valid_up_to: error.valid_up_to(),
        })?;
        Ok(BoundSyntax {
            root: self.root(),
            source: SyntaxSource {
                source: resolved.source(),
                label: resolved.label(),
                text,
            },
        })
    }
}
