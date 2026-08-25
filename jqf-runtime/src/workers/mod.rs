//! Explicitly bounded native worker execution.
//!
//! The session budget starts no thread and is not `Send`. Grants size one worker's numeric envelope. Native is the
//! scoped spawn seam. Ordered releases morsels in ordinal order. A morsel descriptor is the clean outcome a worker may
//! send.

mod budget;
mod grants;
mod morsel;
mod native;
mod ordered;

pub(crate) use budget::{WorkerBudget, WorkerPermit};

pub use grants::{RecordWorkerEnvelope, RecordWorkerGrants, WorkerCountReport, reserve_record_worker_grants};
pub use morsel::{MorselByteRange, MorselFallbackCause, MorselOutcome};
pub(crate) use native::FrontierWake;
pub use native::{NativeWorkerControl, NativeWorkerHost, NativeWorkerPermit, NativeWorkerScope, NativeWorkerTask};
pub use ordered::{
    OrderedRecordCoordinator, OrderedRecordCoordinatorError, OrderedRecordDescriptorContract, OrderedRecordDispatch,
    OrderedRecordPoll, OrderedRecordReady, OrderedRecordSnapshot, OrderedRecordTaskOutput, OrderedRecordTerminal,
    RecordConcurrencyWindow,
};
