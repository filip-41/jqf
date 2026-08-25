//! The ceilings a request is allowed to hit.
//!
//! A request uses every field as the host set it. A worker child reuses the unused input / output / spill words as its
//! memory partitions.

use crate::MemoryCategory;
use core::fmt;

/// Who owns the account's ceilings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
enum AccountKind {
    /// Ordinary request. Every ceiling is what the host set.
    Request = 0,
    /// Worker child. Parent-only ceilings read as unlimited.
    Task = 1,
}

/// Which child memory partition a category counts against.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TaskMemoryGroup {
    /// Retained bytes plus shared-resident cache leases.
    Retained,
    /// Working scratch plus diagnostic buffers.
    Working,
    /// In-flight output / source buffers.
    PendingIo,
}

/// The ceilings for one request.
///
/// Memory categories are labels under one memory ceiling, not separate limits.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct ResourceLimits {
    max_input_bytes: u64,
    max_output_bytes: u64,
    max_memory_bytes: u64,
    max_spill_bytes: u64,
    max_spill_disk_bytes: u64,
    max_nesting_depth: u32,
    account_kind: AccountKind,
    // Probe only: skip memory (and some logical) charges so a measurement can see the ledger's own cost. Not a product
    // path.
    probe_unaccounted: bool,
}

impl fmt::Debug for ResourceLimits {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResourceLimits")
            .field("max_input_bytes", &self.max_input_bytes())
            .field("max_output_bytes", &self.max_output_bytes())
            .field("max_memory_bytes", &self.max_memory_bytes())
            .field("max_spill_bytes", &self.max_spill_bytes())
            .field("max_spill_disk_bytes", &self.max_spill_disk_bytes())
            .field("max_nesting_depth", &self.max_nesting_depth())
            .field("account_kind", &self.account_kind)
            .field("probe_unaccounted", &self.probe_unaccounted)
            .finish()
    }
}

impl ResourceLimits {
    /// Build limits. The spill-disk ceiling starts unset (`0`); turn it on with [`Self::with_max_spill_disk_bytes`].
    #[must_use]
    pub const fn new(
        max_input_bytes: u64,
        max_output_bytes: u64,
        max_memory_bytes: u64,
        max_spill_bytes: u64,
        max_nesting_depth: u32,
    ) -> Self {
        Self {
            max_input_bytes,
            max_output_bytes,
            max_memory_bytes,
            max_spill_bytes,
            // Disk ceiling is opt-in (`0` = unset). Existing callers keep "no disk ceiling" behavior until they ask.
            max_spill_disk_bytes: 0,
            max_nesting_depth,
            account_kind: AccountKind::Request,
            probe_unaccounted: false,
        }
    }

    /// Cap the total bytes this request may write to spill files.
    ///
    /// Charged at each write and never released. `0` means no cap. Leave it unset unless you want a disk ceiling.
    #[must_use]
    pub const fn with_max_spill_disk_bytes(mut self, bytes: u64) -> Self {
        self.max_spill_disk_bytes = bytes;
        self
    }

    /// PROBE-ONLY: switches every memory admission, commit, and release off.
    #[must_use]
    pub fn with_probe_unaccounted(mut self) -> Self {
        self.probe_unaccounted = true;
        self
    }

    /// PROBE-ONLY accessor.
    pub(crate) const fn probe_unaccounted(&self) -> bool {
        self.probe_unaccounted
    }

    /// Whether these limits belong to a worker child.
    pub(crate) const fn is_task(self) -> bool {
        matches!(self.account_kind, AccountKind::Task)
    }

    pub(crate) fn task(
        retained_bytes: u64,
        working_bytes: u64,
        pending_io_bytes: u64,
        max_memory_bytes: u64,
        max_nesting_depth: u32,
    ) -> Self {
        Self {
            // Task children do not independently police logical IO: the coordinator's parent request already owns those
            // limits. These otherwise-unused words encode the child memory partitions without growing every ordinary
            // request ledger.
            max_input_bytes: retained_bytes,
            max_output_bytes: working_bytes,
            max_memory_bytes,
            max_spill_bytes: pending_io_bytes,
            // Task children never police the request's disk ceiling; the parent request owns it.
            max_spill_disk_bytes: 0,
            max_nesting_depth,
            account_kind: AccountKind::Task,
            probe_unaccounted: false,
        }
    }

    /// Maximum input bytes. A worker child returns `u64::MAX`.
    #[must_use]
    pub const fn max_input_bytes(self) -> u64 {
        match self.account_kind {
            AccountKind::Request => self.max_input_bytes,
            AccountKind::Task => u64::MAX,
        }
    }

    /// Maximum output bytes. A worker child returns `u64::MAX`.
    #[must_use]
    pub const fn max_output_bytes(self) -> u64 {
        match self.account_kind {
            AccountKind::Request => self.max_output_bytes,
            AccountKind::Task => u64::MAX,
        }
    }

    /// Maximum live memory, all categories together.
    #[must_use]
    pub const fn max_memory_bytes(self) -> u64 {
        self.max_memory_bytes
    }

    /// Maximum in-memory spill. A worker child returns `u64::MAX`.
    #[must_use]
    pub const fn max_spill_bytes(self) -> u64 {
        match self.account_kind {
            AccountKind::Request => self.max_spill_bytes,
            AccountKind::Task => u64::MAX,
        }
    }

    /// Spill-disk ceiling in bytes. `0` means no ceiling. A worker child returns `u64::MAX`; the parent owns this
    /// limit.
    #[must_use]
    pub const fn max_spill_disk_bytes(self) -> u64 {
        match self.account_kind {
            AccountKind::Request => self.max_spill_disk_bytes,
            AccountKind::Task => u64::MAX,
        }
    }

    /// Maximum nesting depth.
    #[must_use]
    pub const fn max_nesting_depth(self) -> u32 {
        self.max_nesting_depth
    }

    pub(crate) const fn task_memory_group(self, category: MemoryCategory) -> Option<(TaskMemoryGroup, u64)> {
        if !self.is_task() {
            return None;
        }
        Some(match category {
            MemoryCategory::Retained => (TaskMemoryGroup::Retained, self.max_input_bytes),
            MemoryCategory::Working | MemoryCategory::Diagnostic => (TaskMemoryGroup::Working, self.max_output_bytes),
            MemoryCategory::PendingIo => (TaskMemoryGroup::PendingIo, self.max_spill_bytes),
        })
    }
}
