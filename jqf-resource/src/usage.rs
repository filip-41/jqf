//! A copy of how much a request has used, taken at one moment.
//!
//! The account holds the live counters. This file is the read-only view: take a snapshot, read the numbers, throw it
//! away. A snapshot does not allocate and does not change the ledger.

/// A label for why memory is held.
///
/// All labels share one memory ceiling. They exist so a report can say "this many bytes are the input" versus "this
/// many are scratch"; they are not separate budgets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum MemoryCategory {
    /// Input, values, plans, or other request-retained products.
    Retained = 0,
    /// Temporary parser, encoder, evaluator, or builtin working state.
    Working = 1,
    /// Structured errors, labels, and diagnostic rendering state.
    Diagnostic = 2,
    /// Buffers retained by an in-progress source, sink, or detached task result awaiting ordered publication.
    PendingIo = 3,
}

impl MemoryCategory {
    /// Derived from the last variant so adding one cannot desync the fixed-size arrays indexed by [`Self::index`].
    pub(crate) const COUNT: usize = Self::PendingIo as usize + 1;

    pub(crate) const fn index(self) -> usize {
        self as usize
    }
}

/// Current and peak bytes for one memory category.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MemoryUsage {
    pub(crate) current: u64,
    pub(crate) peak: u64,
}

impl MemoryUsage {
    /// Live bytes in this category right now.
    #[must_use]
    pub const fn current(self) -> u64 {
        self.current
    }

    /// Highest live bytes seen in this category.
    #[must_use]
    pub const fn peak(self) -> u64 {
        self.peak
    }
}

/// A copy of every counter at one moment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageSnapshot {
    pub(crate) input_bytes: u64,
    pub(crate) output_bytes: u64,
    pub(crate) output_reserved_bytes: u64,
    pub(crate) spill_disk_bytes: u64,
    pub(crate) memory: [MemoryUsage; MemoryCategory::COUNT],
    pub(crate) memory_current_bytes: u64,
    pub(crate) memory_peak_bytes: u64,
    pub(crate) nesting_depth: u32,
    pub(crate) nesting_peak: u32,
}

impl UsageSnapshot {
    /// Input bytes counted so far.
    #[must_use]
    pub const fn input_bytes(self) -> u64 {
        self.input_bytes
    }

    /// Output bytes committed so far.
    #[must_use]
    pub const fn output_bytes(self) -> u64 {
        self.output_bytes
    }

    /// Output reserved but not yet committed.
    #[must_use]
    pub const fn output_reserved_bytes(self) -> u64 {
        self.output_reserved_bytes
    }

    /// Spill-file bytes written so far.
    #[must_use]
    pub const fn spill_disk_bytes(self) -> u64 {
        self.spill_disk_bytes
    }

    /// Current and peak for one memory category.
    #[must_use]
    pub const fn memory(self, category: MemoryCategory) -> MemoryUsage {
        self.memory[category.index()]
    }

    /// Live memory, all categories together.
    #[must_use]
    pub const fn memory_current_bytes(self) -> u64 {
        self.memory_current_bytes
    }

    /// Highest live memory seen this request.
    #[must_use]
    pub const fn memory_peak_bytes(self) -> u64 {
        self.memory_peak_bytes
    }

    /// Nesting depth right now.
    #[must_use]
    pub const fn nesting_depth(self) -> u32 {
        self.nesting_depth
    }

    /// Highest nesting depth seen this request.
    #[must_use]
    pub const fn nesting_peak(self) -> u32 {
        self.nesting_peak
    }
}
