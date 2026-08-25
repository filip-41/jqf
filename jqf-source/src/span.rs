//! Half-open byte ranges inside one source.
//!
//! A [`Span`] is `[start, end)` as `u32`. It does not know which source those bytes belong to — pair it with
//! [`crate::SourceRef`] for that. Offsets are bytes, not characters.

use core::{fmt, ops::Range};

/// Why a span could not be built.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpanError {
    /// Start is greater than end.
    StartExceedsEnd,
    /// An offset is larger than `u32::MAX`.
    OffsetOverflow,
}

impl fmt::Display for SpanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::StartExceedsEnd => "span start exceeds span end",
            Self::OffsetOverflow => "span offset exceeds u32::MAX",
        })
    }
}

impl core::error::Error for SpanError {}

/// Half-open byte range `[start, end)` inside one source.
///
/// Offsets are `u32`. Pair it with [`crate::SourceRef`] when you need to know which source.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Span {
    start: u32,
    end: u32,
}

impl Span {
    /// Build a span.
    ///
    /// # Panics
    ///
    /// Panics if `start > end`.
    #[must_use]
    pub const fn new(start: u32, end: u32) -> Self {
        match Self::try_new(start, end) {
            Some(span) => span,
            None => panic!("span start exceeds span end"),
        }
    }

    /// Same as [`Self::new`], but returns `None` if `start > end`.
    #[must_use]
    pub const fn try_new(start: u32, end: u32) -> Option<Self> {
        if start <= end { Some(Self { start, end }) } else { None }
    }

    /// Build a span from `usize` offsets.
    ///
    /// Use this when slicing source bytes.
    ///
    /// # Panics
    ///
    /// Panics if `start > end` or an offset does not fit in `u32`.
    #[must_use]
    pub fn from_usize(start: usize, end: usize) -> Self {
        Self::try_from_usize(start, end).unwrap_or_else(|error| panic!("{error}"))
    }

    /// Same as [`Self::from_usize`], but returns the error instead of panicking.
    ///
    /// `start > end` is checked on the `usize` pair first. [`SpanError::OffsetOverflow`] is reported only when the
    /// range is ordered and an offset does not fit in `u32`.
    ///
    /// # Errors
    ///
    /// [`SpanError::StartExceedsEnd`] or [`SpanError::OffsetOverflow`].
    pub fn try_from_usize(start: usize, end: usize) -> Result<Self, SpanError> {
        if start > end {
            return Err(SpanError::StartExceedsEnd);
        }
        let start = u32::try_from(start).map_err(|_| SpanError::OffsetOverflow)?;
        let end = u32::try_from(end).map_err(|_| SpanError::OffsetOverflow)?;
        Ok(Self { start, end })
    }

    /// Inclusive start byte offset.
    #[must_use]
    pub const fn start(self) -> u32 {
        self.start
    }

    /// Exclusive end byte offset.
    #[must_use]
    pub const fn end(self) -> u32 {
        self.end
    }

    /// As a `usize` range, for slicing source bytes.
    #[must_use]
    pub const fn range(self) -> Range<usize> {
        self.start as usize..self.end as usize
    }

    /// Byte length of the span.
    #[must_use]
    pub const fn len(self) -> u32 {
        self.end - self.start
    }

    /// Whether the span is zero-width.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }

    /// Smallest span covering both inputs, gap included.
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

impl fmt::Display for Span {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}..{}", self.start, self.end)
    }
}
