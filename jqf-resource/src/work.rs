//! Work budget for one cooperative slice.
//!
//! [`WorkMeter`] starts each slice with a fresh budget (replace, never add). When the budget is gone, you get
//! [`WorkAdmission::Pending`], not a resource error. Use it through [`crate::ResourceContext`].

/// Maximum budget admitted for one cooperative entry.
pub(crate) const MAX_BUDGET_PER_COOPERATIVE_ENTRY: u32 = 4096;

/// Maximum byte/scalar/collection-slot items covered by one budget unit.
pub(crate) const MAX_LINEAR_ITEMS_PER_UNIT: usize = 256;

/// What the next work request returned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkAdmission {
    /// Do this many items now.
    Granted(usize),
    /// Budget is gone. Come back on the next slice.
    Pending,
}

/// Remaining work budget for this slice. Shared by nested work via `&mut`.
#[derive(Debug)]
pub struct WorkMeter {
    remaining_budget: u32,
}

impl WorkMeter {
    /// Start a slice with this much budget.
    ///
    /// `None` if `budget` is 0 or more than 4096.
    #[must_use]
    #[inline]
    pub const fn try_new_v1(budget: u32) -> Option<Self> {
        if budget == 0 || budget > MAX_BUDGET_PER_COOPERATIVE_ENTRY {
            None
        } else {
            Some(Self {
                remaining_budget: budget,
            })
        }
    }

    /// Budget left in this slice.
    #[must_use]
    #[inline]
    pub const fn remaining(&self) -> u32 {
        self.remaining_budget
    }

    /// Replenishes the meter for the next cooperative entry.
    ///
    /// Returns `false` and leaves the meter unchanged for zero or a count above the maximum budget one entry admits. A
    /// fresh entry replaces, rather than adds to, budget left by an entry that returned early.
    #[inline]
    pub(crate) const fn try_replenish(&mut self, budget: u32) -> bool {
        if budget == 0 || budget > MAX_BUDGET_PER_COOPERATIVE_ENTRY {
            false
        } else {
            self.remaining_budget = budget;
            true
        }
    }

    /// Admits at most 256 linear items (bytes, or collection slots).
    #[inline]
    pub(crate) fn admit_linear_items(&mut self, remaining: usize) -> WorkAdmission {
        self.admit_bounded(remaining, MAX_LINEAR_ITEMS_PER_UNIT)
    }

    /// Admits one lexer token, parser/state-machine transition, node, occurrence, fact, record, row, or column vector.
    #[inline]
    pub(crate) fn admit_transition(&mut self) -> WorkAdmission {
        self.admit_bounded(1, 1)
    }

    /// Admits as many individually-accounted transitions as the current entry can pay for.
    #[inline]
    pub(crate) fn admit_transitions(&mut self, remaining: usize) -> WorkAdmission {
        if remaining == 0 {
            return WorkAdmission::Granted(0);
        }
        if self.remaining_budget == 0 {
            return WorkAdmission::Pending;
        }
        let granted = self.remaining_budget.min(u32::try_from(remaining).unwrap_or(u32::MAX));
        self.remaining_budget -= granted;
        WorkAdmission::Granted(granted as usize)
    }

    /// Returns unused budget from a grant that finished early.
    ///
    /// A 5-step encode that admitted the rest of the slice would otherwise empty the meter and force a cooperative
    /// re-entry on the next item.
    #[inline]
    pub(crate) fn refund(&mut self, unused: u32) {
        self.remaining_budget = self
            .remaining_budget
            .saturating_add(unused)
            .min(MAX_BUDGET_PER_COOPERATIVE_ENTRY);
    }

    #[inline]
    fn admit_bounded(&mut self, remaining: usize, maximum: usize) -> WorkAdmission {
        if remaining == 0 {
            return WorkAdmission::Granted(0);
        }
        if self.remaining_budget == 0 {
            return WorkAdmission::Pending;
        }
        self.remaining_budget -= 1;
        WorkAdmission::Granted(remaining.min(maximum))
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_BUDGET_PER_COOPERATIVE_ENTRY, WorkMeter};

    #[test]
    fn invalid_initial_budgets_are_refused() {
        assert!(WorkMeter::try_new_v1(0).is_none());
        assert!(WorkMeter::try_new_v1(MAX_BUDGET_PER_COOPERATIVE_ENTRY + 1).is_none());
    }

    #[test]
    fn invalid_replenishment_leaves_the_budget_unchanged() {
        let mut meter = WorkMeter::try_new_v1(7).expect("valid budget");
        assert!(!meter.try_replenish(0));
        assert_eq!(meter.remaining(), 7);
        assert!(!meter.try_replenish(MAX_BUDGET_PER_COOPERATIVE_ENTRY + 1));
        assert_eq!(meter.remaining(), 7);
    }
}
