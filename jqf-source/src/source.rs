//! Who a span belongs to, and how the bytes are carried.
//!
//! A [`SourceRef`] is an id plus a kind (`query` or `input`). A [`ResolvedSource`] carries the borrowed bytes one
//! caller retained for that ref. [`SourceFileRange`] names one file inside a concatenated input.

use core::fmt;

/// Numeric id of one source.
///
/// The same number can name two sources: pair it with [`SourceKind`] to form a [`SourceRef`].
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceId(u32);

impl SourceId {
    /// Build a source id.
    #[must_use]
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    /// The number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// What kind of source a [`SourceRef`] names.
///
/// [`Self::Query`] is the program. [`Self::Input`] is the document. Streaming records are still `Input`, with a
/// per-record [`ResolvedSource::base_offset`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SourceKind {
    /// The program text.
    Query,
    /// The document being processed.
    Input,
}

impl SourceKind {
    /// `"query"` or `"input"`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Input => "input",
        }
    }
}

/// One source: an id plus a [`SourceKind`].
///
/// This is what labels carry and what a [`ResolvedSource`] is built from. It is not a path and not a buffer. Same id,
/// different kind → two sources (`query#0` vs `input#0`).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceRef {
    id: SourceId,
    kind: SourceKind,
}

impl SourceRef {
    /// Create a source reference.
    #[must_use]
    pub const fn new(id: SourceId, kind: SourceKind) -> Self {
        Self { id, kind }
    }

    /// Source id.
    #[must_use]
    pub const fn id(self) -> SourceId {
        self.id
    }

    /// Source kind.
    #[must_use]
    pub const fn kind(self) -> SourceKind {
        self.kind
    }
}

impl fmt::Display for SourceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.kind.as_str(), self.id.get())
    }
}

/// One file inside a concatenated input: its name and `[start, end)` in the combined bytes.
///
/// Not a [`crate::Span`]: offsets are `u64` because a concat can be larger than `u32`, and the range is stored as given
/// (not checked). You keep the ranges butted together in argument order. If a value crosses a file boundary, attribute
/// it to the file that contains its last byte.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceFileRange<'a> {
    label: &'a str,
    start: u64,
    end: u64,
}

impl<'a> SourceFileRange<'a> {
    /// Build a file range. `start` and `end` are stored as given.
    #[must_use]
    pub const fn new(label: &'a str, start: u64, end: u64) -> Self {
        Self { label, start, end }
    }

    /// File name (or other label) for a value ending in this file.
    #[must_use]
    pub const fn label(&self) -> &'a str {
        self.label
    }

    /// First byte offset of this file in the combined source.
    #[must_use]
    pub const fn start(&self) -> u64 {
        self.start
    }

    /// First byte offset AFTER this file in the combined source.
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.end
    }
}

/// Borrowed source bytes, plus a label and a base offset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedSource<'source> {
    source: SourceRef,
    label: &'source str,
    bytes: &'source [u8],
    base_offset: u64,
}

impl<'source> ResolvedSource<'source> {
    /// Build a resolved source.
    #[must_use]
    pub const fn new(source: SourceRef, label: &'source str, bytes: &'source [u8], base_offset: u64) -> Self {
        Self {
            source,
            label,
            bytes,
            base_offset,
        }
    }

    /// Source identity.
    #[must_use]
    pub const fn source(&self) -> SourceRef {
        self.source
    }

    /// Display label (`"stdin"`, a filename, …).
    #[must_use]
    pub const fn label(&self) -> &'source str {
        self.label
    }

    /// Source bytes.
    #[must_use]
    pub const fn bytes(&self) -> &'source [u8] {
        self.bytes
    }

    /// Byte offset of the start of these bytes in the original source.
    ///
    /// Still set when the slice is empty.
    #[must_use]
    pub const fn base_offset(&self) -> u64 {
        self.base_offset
    }
}
