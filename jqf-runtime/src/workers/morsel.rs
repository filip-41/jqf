//! Closed, worker-safe descriptors for one ordered morsel.
//!
//! A MORSEL is a contiguous RANGE of records sized by bytes, never a single record: per-record dispatch pays a
//! decode-and-handoff cost per record that byte-range dispatch amortizes across the whole range, and a per-record
//! worker would pay a thread handoff on top. One coordinator ordinal is therefore one morsel.
//!
//! Every type here is `Copy`, contains no allocation, no account, no document, and no codec error, so it can cross a
//! worker boundary while the worker's dynamic bytes stay in its detached task buffer until the parent adopts them. A
//! morsel worker runs the same record drive the serial path uses — there is no second interpreter.

/// One checked half-open byte range inside a worker's detached result buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MorselByteRange {
    start: u64,
    end: u64,
}

impl MorselByteRange {
    /// Creates one half-open range when `start <= end`.
    #[must_use]
    pub const fn try_new(start: u64, end: u64) -> Option<Self> {
        if start <= end { Some(Self { start, end }) } else { None }
    }

    /// Returns the inclusive start offset in the detached buffer.
    #[must_use]
    pub const fn start(self) -> u64 {
        self.start
    }

    /// Returns the exclusive end offset in the detached buffer.
    #[must_use]
    pub const fn end(self) -> u64 {
        self.end
    }

    /// Returns the exact range length.
    // No consumer ever asks a byte range for emptiness, so the paired predicate stays absent.
    #[allow(
        clippy::len_without_is_empty,
        reason = "a closed byte range has no empty case worth naming"
    )]
    #[must_use]
    pub const fn len(self) -> u64 {
        self.end - self.start
    }

    /// Returns whether this range lies inside a buffer of `buffer_len` bytes.
    #[must_use]
    pub const fn fits_within(self, buffer_len: u64) -> bool {
        self.end <= buffer_len
    }
}

/// Closed descriptor of one morsel that completed CLEANLY.
///
/// # What "clean" means, and why the coordinator only carries clean morsels
///
/// A clean morsel published bytes and NOTHING else: no record issue, no per-value runtime error, no decode failure.
/// Serial's observable behaviour for such a range is exactly its published bytes, so the parent can relay them verbatim
/// and the byte-identity law holds by construction.
///
/// Every other outcome — an issue, a reported value error, a decode failure, a worker fault — is NOT described here. It
/// becomes the coordinator's ordered TERMINAL ([`MorselFallbackCause`]), because reproducing serial's diagnostics,
/// input line numbers, and exit class from a worker that saw only a byte range of the stream is not something a
/// descriptor can promise.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MorselOutcome {
    bytes: MorselByteRange,
    records: u64,
    items: u64,
}

impl MorselOutcome {
    /// Creates one clean morsel outcome over `bytes`, covering `records` and publishing `items`.
    #[must_use]
    pub const fn new(bytes: MorselByteRange, records: u64, items: u64) -> Self {
        Self { bytes, records, items }
    }

    /// Returns the ordered items this morsel published.
    #[must_use]
    pub const fn items(self) -> u64 {
        self.items
    }

    /// Returns the published byte range inside the adopted buffer.
    #[must_use]
    pub const fn bytes(self) -> MorselByteRange {
        self.bytes
    }

    /// Returns the records this morsel decoded and ran.
    #[must_use]
    pub const fn records(self) -> u64 {
        self.records
    }

    /// Validates the referenced byte range against an adopted buffer.
    #[must_use]
    pub const fn fits_buffer(self, buffer_len: u64) -> bool {
        self.bytes.fits_within(buffer_len)
    }
}

/// Why a morsel could not be relayed, and where serial must take over.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MorselFallbackCause {
    /// The morsel drive reported a record issue or a per-value runtime error.
    ///
    /// Both are ORDERED diagnostics whose text carries absolute input line numbers, so only a drive that sees the whole
    /// stream may render them.
    Diagnostics,
    /// The morsel drive failed: a malformed payload, an exhausted grant, or any other pipeline failure.
    DriveFailed,
    /// The worker faulted, was cancelled, or its grant was refused.
    WorkerUnavailable,
}
