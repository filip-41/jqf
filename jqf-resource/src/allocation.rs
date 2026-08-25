//! Byte buffers that the counting allocator already charged.
//!
//! Worker output is a raw `u8` block plus its category label. The owner commits exact capacity once and appends only
//! within that allocation.
//!
//! [`DepthGuard`] is also here: one nesting level, released on drop.

use crate::{MemoryCategory, ResourceError};
use core::ptr::NonNull;
use std::alloc::{Layout, alloc, dealloc};

/// Exact raw allocation carrying the category label of its ambient charge.
///
/// Only `u8` is allocated: [`crate::task::TaskOutputBuffer`] is the sole owner.
pub(crate) struct RawAllocation {
    pointer: NonNull<u8>,
    residency: Option<(Layout, MemoryCategory)>,
    charged_account: Option<usize>,
}

impl RawAllocation {
    #[inline]
    pub(crate) fn try_for_array(capacity: usize, category: MemoryCategory) -> Result<Self, ResourceError> {
        let layout = Layout::array::<u8>(capacity).map_err(|_| ResourceError::ArithmeticOverflow)?;
        Self::try_for_layout(layout, category)
    }

    fn try_for_layout(layout: Layout, category: MemoryCategory) -> Result<Self, ResourceError> {
        if layout.size() == 0 {
            return Ok(Self {
                pointer: NonNull::dangling(),
                residency: None,
                charged_account: None,
            });
        }

        // The counting allocator charges this allocation against the thread's ambient account; the category scope
        // routes the charge into `category`. A refused charge makes `alloc` return null, which is the GlobalAlloc
        // contract's refusal — no explicit ledger admission exists to double it.
        let _category = crate::ambient::category_scope(category);
        let charged_account = crate::ambient::with_account(crate::account::RequestAccount::account_address);
        // SAFETY: `layout` is non-zero and valid. A non-null result is owned
        // uniquely by the returned allocation and deallocated with the same layout in `Drop` or by the owner returned
        // from `into_nonzero_parts`.
        let raw = unsafe { alloc(layout) };
        let Some(pointer) = NonNull::new(raw) else {
            return Err(ResourceError::AllocationFailed);
        };
        Ok(Self {
            pointer,
            residency: Some((layout, category)),
            charged_account,
        })
    }

    #[inline]
    pub(crate) const fn pointer(&self) -> NonNull<u8> {
        self.pointer
    }

    #[inline]
    pub(crate) fn resident_bytes(&self) -> u64 {
        self.residency
            .as_ref()
            .map_or(0, |(layout, _)| u64::try_from(layout.size()).unwrap_or(u64::MAX))
    }

    #[inline]
    pub(crate) fn into_nonzero_parts(
        mut self,
    ) -> Result<(NonNull<u8>, Layout, MemoryCategory, Option<usize>), ResourceError> {
        let Some((layout, category)) = self.residency.take() else {
            return Err(ResourceError::AccountingInvariantViolation);
        };
        Ok((self.pointer, layout, category, self.charged_account.take()))
    }
}

impl Drop for RawAllocation {
    fn drop(&mut self) {
        if let Some((layout, category)) = self.residency.take() {
            // The dealloc releases the ambient charge in the same category the alloc charged instead of releasing the
            // thread's current category.
            let _category = crate::ambient::category_scope(category);
            // SAFETY: this owner received `pointer` from `alloc(layout)`, and
            // no other path deallocates it while `layout` remains armed.
            unsafe { dealloc(self.pointer.as_ptr(), layout) };
        }
    }
}

/// One nesting level. Drop it to leave.
///
/// ```compile_fail use jqf_resource::DepthGuard; fn assert_send<T: Send>() {} assert_send::<DepthGuard>();
/// ```
#[derive(Debug)]
#[must_use = "a depth guard defines the lifetime of one nested scope"]
pub struct DepthGuard<'a> {
    state: &'a crate::account::AccountState,
}

impl<'a> DepthGuard<'a> {
    #[inline]
    pub(crate) const fn new(state: &'a crate::account::AccountState) -> Self {
        Self { state }
    }
}

impl Drop for DepthGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.state.leave_depth();
    }
}

/// One nesting level that can live on a stored parse/encode frame.
///
/// Prefer [`DepthGuard`] for a lexical scope. Iterative parsers keep this form when an open container survives across
/// poll calls.
#[derive(Debug)]
#[must_use = "an owned depth guard defines the lifetime of one nested scope"]
pub struct OwnedDepthGuard {
    state: Option<crate::account::AccountHandle>,
}

impl OwnedDepthGuard {
    #[inline]
    pub(crate) const fn new(state: crate::account::AccountHandle) -> Self {
        Self { state: Some(state) }
    }
}

impl Drop for OwnedDepthGuard {
    #[inline]
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            state.leave_depth();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RawAllocation;
    use crate::{MemoryCategory, RequestAccount, ResourceLimits};

    #[test]
    fn a_zero_capacity_allocation_is_empty() {
        let allocation = RawAllocation::try_for_array(0, MemoryCategory::PendingIo).expect("zero");
        assert_eq!(allocation.resident_bytes(), 0);
    }

    #[test]
    fn an_owned_depth_guard_increments_and_drop_returns_the_level() {
        let account =
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, 2)).expect("account");
        assert_eq!(account.snapshot().nesting_depth(), 0);
        let first = crate::account::AccountState::enter_depth_owned(&account.state).expect("enter");
        assert_eq!(account.snapshot().nesting_depth(), 1);
        drop(first);
        assert_eq!(account.snapshot().nesting_depth(), 0);
    }

    #[test]
    fn a_depth_guard_increments_and_drop_returns_the_level() {
        let account =
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, 2)).expect("account");
        assert_eq!(account.snapshot().nesting_depth(), 0);
        let first = crate::account::AccountState::enter_depth(&account.state).expect("enter");
        assert_eq!(account.snapshot().nesting_depth(), 1);
        drop(first);
        assert_eq!(account.snapshot().nesting_depth(), 0);
    }
}
