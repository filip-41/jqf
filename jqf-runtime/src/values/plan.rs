//! The adjacent-value route's observed width policy.
//!
//! The plan vocabulary is `crate::parallel::plan`'s. What lives here is the evidence, and it is this route's OWN: the
//! record route's break-even was derived from a framed stream whose cost model — one framing pass, one payload decode
//! per record — is not this route's, so reusing its number would be claiming evidence this lane does not have.

use crate::parallel::{ParallelPlan, PlanDecision, WidthPolicy, WorkerRequest};

/// Below this many input bytes, `auto` stays on the serial path.
///
/// observed for THIS route, not inherited from the record route's 262 144 — which happens to be the same number, and
/// would have been a guess if it had been copied. Sweeping the adjacent-value family at 31 reps per size brackets the
/// crossover between 148 580 bytes (every width within the lane's own 1.1 % noise floor) and 153 780 bytes (4.4-7.5 %
/// ahead of serial); the pin sits at 1.73x that midpoint, keeping the record route's margin DISCIPLINE while carrying
/// its own evidence. The tolerance is one-sided for the reason it exists: the promise is "the default on a small input
/// is never slower than serial", the crossover moves with value shape and machine load, and the only cost of the margin
/// is a missed win between 151 KB and 256 KiB.
pub(crate) const AUTO_BREAK_EVEN_BYTES: u64 = 256 * 1024;

/// The narrowest width `auto` will ever choose above the break-even point.
///
/// observed: two shards run the 5.78 MB lane at 0.590x serial for 1.05x the total CPU (0.11 s wall / 0.20 s user
/// against serial's 0.19 s / 0.18 s), so there is no width between serial and two that could be preferable. Same
/// conclusion as the record route's, reached on this route's own sweep rather than transferred.
const AUTO_MIN_WORKERS: usize = 2;

/// The smallest shard the lane will cut, and so `auto`'s unit of width.
///
/// A shard is a RANGE of adjacent values sized by bytes, never one value. Below this the per-shard cost — one thread
/// spawn, one grant, one requirement lowering, one provider — stops amortizing over the values inside it.
///
/// Because this floor binds on every input the break-even admits, it is also what decides how many workers can be
/// USEFULLY employed: an input yields at most `ceil(input / MIN_SHARD_BYTES)` shards at the floor, and a worker beyond
/// that count draws nothing.
const MIN_SHARD_BYTES: u64 = 128 * 1024;

/// The largest shard the lane will cut.
///
/// The worker envelope is sized from the shard, so this bounds one worker's memory: a wide request over a huge input
/// divides into more shards rather than into fatter ones.
const MAX_SHARD_BYTES: u64 = 4 * 1024 * 1024;

/// Shards per worker the planner aims for.
///
/// More than one, so a worker that draws a slow shard does not strand the tail; few enough that dispatch and per-shard
/// allocation stay amortized. This route's shard sizes are NOT uniform — a shard grows past its target to reach the
/// next provable boundary — so the tail margin matters here at least as much as it does on the framed route, and the
/// record route's observed 4 is adopted rather than re-tuned.
const SHARDS_PER_WORKER: u64 = 4;

/// The adjacent-value route's pinned width policy.
pub(crate) const VALUE_WIDTH_POLICY: WidthPolicy = WidthPolicy {
    break_even_bytes: AUTO_BREAK_EVEN_BYTES,
    min_workers: AUTO_MIN_WORKERS,
    min_morsel_bytes: MIN_SHARD_BYTES,
    max_morsel_bytes: MAX_SHARD_BYTES,
    morsels_per_worker: SHARDS_PER_WORKER,
};

/// Plans one adjacent-value request under this route's own policy.
#[must_use]
pub fn plan_request(request: WorkerRequest, input_bytes: u64, ineligible: Option<PlanDecision>) -> ParallelPlan {
    crate::parallel::plan_request(request, input_bytes, ineligible, VALUE_WIDTH_POLICY)
}

#[cfg(test)]
mod tests {
    use super::{AUTO_BREAK_EVEN_BYTES, MIN_SHARD_BYTES, PlanDecision, WorkerRequest, plan_request};
    use crate::parallel::auto_worker_ceiling;

    #[test]
    fn auto_stays_serial_below_the_pinned_break_even() {
        let plan = plan_request(WorkerRequest::Auto, AUTO_BREAK_EVEN_BYTES - 1, None);
        assert_eq!(plan.decision(), PlanDecision::BelowBreakEven);
        assert_eq!(plan.workers(), 0);
    }

    #[test]
    fn auto_asks_for_one_worker_per_drawable_shard_within_its_bounds() {
        let plan = plan_request(WorkerRequest::Auto, AUTO_BREAK_EVEN_BYTES, None);
        assert_eq!(plan.decision(), PlanDecision::Parallel);
        assert_eq!(plan.workers(), 2);

        let ceiling = auto_worker_ceiling();
        let plan = plan_request(WorkerRequest::Auto, 4096 * MIN_SHARD_BYTES, None);
        assert_eq!(plan.workers(), ceiling);
    }

    #[test]
    fn an_ineligible_request_records_why_it_fell_through() {
        let plan = plan_request(WorkerRequest::Auto, 1 << 30, Some(PlanDecision::ProgramIneligible));
        assert_eq!(plan.decision(), PlanDecision::ProgramIneligible);
        assert!(!plan.is_parallel());
    }
}
