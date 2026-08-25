//! Session-owned worker permits for native orchestration.
//!
//! The budget owns no pool and starts no work. A zero-capacity budget is the portable serial configuration.
//! [`WorkerPermit`] is neither `Clone` nor `Send`: the coordinator holds it until the task joins. Sibling of
//! [`super::native`] (which wraps this budget) and [`super::grants`] (memory envelopes, not thread slots).

use std::cell::Cell;
use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;

/// Request-local ceiling for native work that may be in flight concurrently.
///
/// The budget owns no pool and starts no work. A native host borrows one [`WorkerPermit`] per task and keeps that
/// permit on the coordinator until the task has joined.
///
/// Worker capacity is supplied explicitly by the host or session. Portable code never reads process environment or
/// machine topology.
///
/// The type is deliberately not `Send` (and [`WorkerPermit`] neither `Send` nor `Clone`): the `PhantomData<Rc<()>>`
/// marker makes that structural, so a permit can never cross to another thread and drop its slot there.
#[derive(Debug)]
pub struct WorkerBudget {
    capacity: usize,
    in_flight: Cell<usize>,
    request_local: PhantomData<Rc<()>>,
}

impl WorkerBudget {
    /// Creates a session budget with exactly `capacity` worker permits.
    ///
    /// A zero-capacity budget is the portable serial configuration.
    #[must_use]
    pub const fn new(capacity: usize) -> Self {
        Self {
            capacity,
            in_flight: Cell::new(0),
            request_local: PhantomData,
        }
    }

    /// Returns the currently available permit count.
    #[must_use]
    pub fn available(&self) -> usize {
        self.capacity.saturating_sub(self.in_flight.get())
    }

    /// Borrows one permit without waiting.
    ///
    /// `None` means the session must keep work local, defer dispatch, or take another already-proven route. Dropping
    /// the returned permit releases exactly one slot.
    #[must_use]
    pub fn try_acquire(&self) -> Option<WorkerPermit<'_>> {
        let current = self.in_flight.get();
        if current >= self.capacity {
            return None;
        }
        self.in_flight.set(current + 1);
        Some(WorkerPermit {
            budget: self,
            request_local: PhantomData,
        })
    }
}

/// One coordinator-local claim on a session worker slot.
///
/// It intentionally implements neither `Clone` nor `Copy`, and is not `Send`. The native coordinator retains it while
/// the worker receives only its numeric [`jqf_resource::task::TaskGrantBudget`].
#[must_use = "dropping a worker permit releases its session slot"]
pub struct WorkerPermit<'budget> {
    budget: &'budget WorkerBudget,
    request_local: PhantomData<Rc<()>>,
}

impl fmt::Debug for WorkerPermit<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkerPermit")
            .field("capacity", &self.budget.capacity)
            .field("in_flight", &self.budget.in_flight.get())
            .finish_non_exhaustive()
    }
}

impl Drop for WorkerPermit<'_> {
    fn drop(&mut self) {
        let current = self.budget.in_flight.get();
        // Drop cannot return an error, so a double-drop or over-release cannot surface as a codec error. Debug builds
        // flag it; release builds saturate so the counter can never underflow.
        debug_assert!(current > 0, "a live worker permit owns one slot");
        self.budget.in_flight.set(current.saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use super::WorkerBudget;

    #[test]
    fn zero_capacity_never_lends_a_worker() {
        let workers = WorkerBudget::new(0);

        assert_eq!(workers.available(), 0);
        assert!(workers.try_acquire().is_none());
    }

    #[test]
    fn live_permits_bound_concurrency_and_release_exactly_once() {
        let workers = WorkerBudget::new(2);
        let first = workers.try_acquire().expect("first permit");
        let second = workers.try_acquire().expect("second permit");

        assert_eq!(workers.available(), 0);
        assert!(workers.try_acquire().is_none(), "a live permit must consume one slot");

        drop(first);
        assert_eq!(workers.available(), 1);

        let replacement = workers.try_acquire().expect("released slot");
        assert_eq!(workers.available(), 0);

        drop(replacement);
        drop(second);
        assert_eq!(workers.available(), 2);
    }
}
