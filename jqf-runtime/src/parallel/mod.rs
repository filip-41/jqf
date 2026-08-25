//! What every morsel-parallel route shares: the printable plan and the ordered relay that turns clean morsels into
//! serial's own byte stream.
//!
//! Two routes mount on this: `crate::records` (explicit NDJSON, whose morsels are record ranges cut at line feeds the
//! framer guarantees) and `crate::values` (the default adjacent-JSON stdin, whose shard starts must be DISCOVERED).
//! They differ in how a boundary is found and in what one drive decodes; they do not differ in the ordering law, the
//! grant substrate, or the yield-to-serial recovery, so those live here exactly once.

mod plan;
mod relay;

#[cfg(test)]
pub(crate) use plan::auto_worker_ceiling;
pub use plan::{CoreTopology, ParallelPlan, PcoreSource, PlanDecision, WORKER_HARD_CAP, WorkerRequest, core_topology};
pub(crate) use plan::{Morsel, WidthPolicy, plan_request};
pub(crate) use relay::{
    MorselDrive, MorselSink, MorselSource, MorselUnits, Relay, SkippingSink, SliceMorsels, relay_morsels,
};
