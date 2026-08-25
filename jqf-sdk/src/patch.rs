//! Byte replacement primitives for the source-edit route.
//!
//! This module was moved out of `jqf-source` because it is not source
//! identity or diagnostic vocabulary: it is a byte-rewriting utility whose
//! only consumer is this crate's edit route (`execute_source_edit`). Its
//! coordinates are still [`Span`]s, which is why it imports them.

use core::fmt;
use std::vec::Vec;

use jqf_source::{SourceRef, Span, SpanError};

/// Patch construction or application failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatchError {
    /// A supplied range's start exceeds its end.
    StartExceedsEnd,
    /// A compact offset or original length exceeds `u32`.
    OffsetOverflow,
    /// The input length differs from the validated original length.
    OriginalLengthMismatch,
    /// The application source identity differs from the patch source.
    SourceMismatch,
    /// A patch extends beyond the original bytes.
    OutOfBounds,
    /// Patches are not ordered by start offset.
    Unsorted,
    /// Two replacement ranges overlap.
    Overlap,
    /// Same-start operations are ambiguous because at least one is an insertion.
    AmbiguousInsertion,
    /// The resulting byte length cannot be represented safely.
    OutputSizeOverflow,
    /// Allocation for the complete output failed.
    AllocationFailure,
}

impl fmt::Display for PatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::StartExceedsEnd => "patch range start exceeds end",
            Self::OffsetOverflow => "patch offset or original length exceeds u32::MAX",
            Self::OriginalLengthMismatch => "patch original length does not match input",
            Self::SourceMismatch => "patch source identity does not match input",
            Self::OutOfBounds => "patch range exceeds the original bytes",
            Self::Unsorted => "patches are not ordered by start offset",
            Self::Overlap => "patch ranges overlap",
            Self::AmbiguousInsertion => "same-offset patch operations are ambiguous",
            Self::OutputSizeOverflow => "patched output size cannot be represented",
            Self::AllocationFailure => "allocation for patched output failed",
        })
    }
}

impl core::error::Error for PatchError {}

/// One replacement over a half-open compact byte span.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BytePatch {
    span: Span,
    replacement: Vec<u8>,
}
impl BytePatch {
    /// Create a patch from external offsets.
    ///
    /// # Errors
    ///
    /// Returns [`PatchError::StartExceedsEnd`] or [`PatchError::OffsetOverflow`] for an invalid range.
    pub fn try_from_usize(start: usize, end: usize, replacement: Vec<u8>) -> Result<Self, PatchError> {
        let span = Span::try_from_usize(start, end).map_err(|error| match error {
            SpanError::StartExceedsEnd => PatchError::StartExceedsEnd,
            SpanError::OffsetOverflow => PatchError::OffsetOverflow,
        })?;
        Ok(Self { span, replacement })
    }
    /// Replaced span.
    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

/// Validated ordered patches for one original source segment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatchSet {
    source: Option<SourceRef>,
    original_len: u32,
    patches: Vec<BytePatch>,
}
impl PatchSet {
    /// Validate a patch set.
    ///
    /// # Errors
    ///
    /// Returns the first validation failure in the documented deterministic order.
    pub fn try_new(
        source: Option<SourceRef>,
        original_len: usize,
        patches: Vec<BytePatch>,
    ) -> Result<Self, PatchError> {
        let original_len = u32::try_from(original_len).map_err(|_| PatchError::OffsetOverflow)?;
        for patch in &patches {
            if patch.span.end() > original_len {
                return Err(PatchError::OutOfBounds);
            }
        }
        for pair in patches.windows(2) {
            let left = &pair[0];
            let right = &pair[1];
            if right.span.start() < left.span.start() {
                return Err(PatchError::Unsorted);
            }
            if right.span.start() == left.span.start() {
                return if left.span.is_empty() || right.span.is_empty() {
                    Err(PatchError::AmbiguousInsertion)
                } else {
                    Err(PatchError::Overlap)
                };
            }
            if right.span.start() < left.span.end() {
                return Err(PatchError::Overlap);
            }
        }
        Ok(Self {
            source,
            original_len,
            patches,
        })
    }
    /// Apply all replacements after validating source identity and input length.
    ///
    /// # Errors
    ///
    /// Returns a mismatch, output-size, or allocation error before emitting partial output.
    pub fn apply(&self, source: Option<SourceRef>, original: &[u8]) -> Result<Vec<u8>, PatchError> {
        if original.len() != self.original_len as usize {
            return Err(PatchError::OriginalLengthMismatch);
        }
        if self.source != source {
            return Err(PatchError::SourceMismatch);
        }
        let mut removed = 0_usize;
        let mut replacements = 0_usize;
        for patch in &self.patches {
            removed = removed
                .checked_add(patch.span.len() as usize)
                .ok_or(PatchError::OutputSizeOverflow)?;
            replacements = replacements
                .checked_add(patch.replacement.len())
                .ok_or(PatchError::OutputSizeOverflow)?;
        }
        let output_len = checked_output_size(original.len(), removed, replacements)?;
        let mut output = Vec::new();
        output
            .try_reserve_exact(output_len)
            .map_err(|_| PatchError::AllocationFailure)?;
        let mut cursor = 0;
        for patch in &self.patches {
            output.extend_from_slice(&original[cursor..patch.span.start() as usize]);
            output.extend_from_slice(&patch.replacement);
            cursor = patch.span.end() as usize;
        }
        output.extend_from_slice(&original[cursor..]);
        Ok(output)
    }
}

fn checked_output_size(original_len: usize, removed: usize, replacements: usize) -> Result<usize, PatchError> {
    let output_len = original_len
        .checked_sub(removed)
        .and_then(|remaining| remaining.checked_add(replacements))
        .ok_or(PatchError::OutputSizeOverflow)?;
    if output_len > u32::MAX as usize {
        Err(PatchError::OutputSizeOverflow)
    } else {
        Ok(output_len)
    }
}

#[cfg(test)]
mod tests {
    use super::{PatchError, checked_output_size};

    #[test]
    fn complete_output_size_ignores_invalid_intermediate_ordering() {
        assert_eq!(checked_output_size(u32::MAX as usize, 2, 2), Ok(u32::MAX as usize));
        assert_eq!(
            checked_output_size(u32::MAX as usize, 0, 1),
            Err(PatchError::OutputSizeOverflow)
        );
        assert_eq!(
            checked_output_size(usize::MAX, 0, 1),
            Err(PatchError::OutputSizeOverflow)
        );
    }
}
