//! Syntax input representation boundaries.
//!
//! [`MAX_SYNTAX_SOURCE_BYTES`] is a representation limit: source past it cannot be addressed by compact
//! [`jqf_source::Span`]s, so every public entry point refuses it before lexing as [`SyntaxInputError::SourceTooLarge`]
//! — a representation failure, never a recoverable grammar diagnostic. [`MAX_SYNTAX_NESTING_DEPTH`] is the parser's
//! fixed recursion ceiling, shared with the tree-wide document nesting ceiling so one refusal spelling covers both.

use core::fmt;

/// Largest source byte length representable by jqf's compact [`jqf_source::Span`].
pub const MAX_SYNTAX_SOURCE_BYTES: usize = u32::MAX as usize;

/// Deepest program nesting the parser will build, in levels.
///
/// The grammar is recursive descent and the syntax tree it builds is walked recursively again by every later stage, so
/// program nesting is native STACK depth in a way document nesting is not (the codecs drive their frames through heap
/// vectors). Without a ceiling, ~18,000 nested constructors exhausted even the CLI's 256 MiB request stack and aborted
/// the process. The ceiling bounds LEVELS, not stack bytes: it assumes a CLI-class parse thread, and an FFI/embedder
/// host parsing on a smaller thread stack must size that thread for the same depth or refuse deep programs itself.
///
/// The number is the one this tree already uses for document nesting (`ResourceLimits::max_nesting_depth`). A LEVEL is
/// one nested expression, one nested binding pattern, or one link of an operator chain — a chain builds exactly one
/// tree level per link, so a flat `1+1+1…` is as deep a tree as the same number of brackets and costs the later walks
/// the same stack.
pub const MAX_SYNTAX_NESTING_DEPTH: u32 = 10_000;

/// Failure to admit source text into the syntax layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SyntaxInputError {
    /// Source text is longer than compact syntax spans can represent.
    SourceTooLarge {
        /// Largest representable source byte length.
        limit: usize,
        /// Supplied source byte length.
        attempted: usize,
    },
}

impl fmt::Display for SyntaxInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceTooLarge { limit, attempted } => write!(
                formatter,
                "syntax source has {attempted} bytes, exceeding the representable limit of {limit}"
            ),
        }
    }
}

impl core::error::Error for SyntaxInputError {}

pub(crate) const fn validate_source_len(attempted: usize) -> Result<(), SyntaxInputError> {
    // On a 32-bit `usize` (wasm32) u32::MAX IS usize::MAX, so this comparison is tautologically false there and clippy
    // denies it; the limit still holds by construction because no source can exceed it. The check itself stays for
    // every wider target.
    #[allow(clippy::absurd_extreme_comparisons)]
    if attempted > MAX_SYNTAX_SOURCE_BYTES {
        Err(SyntaxInputError::SourceTooLarge {
            limit: MAX_SYNTAX_SOURCE_BYTES,
            attempted,
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_source_length_accepts_its_exact_boundary() {
        assert_eq!(validate_source_len(MAX_SYNTAX_SOURCE_BYTES), Ok(()));
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn compact_source_length_rejects_the_first_unrepresentable_byte() {
        let attempted = MAX_SYNTAX_SOURCE_BYTES + 1;
        assert_eq!(
            validate_source_len(attempted),
            Err(SyntaxInputError::SourceTooLarge {
                limit: MAX_SYNTAX_SOURCE_BYTES,
                attempted,
            })
        );
    }
}
