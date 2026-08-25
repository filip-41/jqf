//! Request limits, usage accounting, and cooperative work budgets.
//!
//! Also: the counting allocator, cancellation, and worker grants. Links `std`. Does not parse, evaluate, encode, or
//! measure process RSS.
//!
//! Unlike `jqf-source`, this crate cannot be `no_std` and cannot forbid `unsafe`: the allocator, the thread-local
//! account, and the emergency slab need both.

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

// The gate names 64-bit atomics because that is the widest width this crate uses (the grant-identity counter). A target
// missing pointer-width atomics would still fail to build, on the slab bump, with rustc's own message.
#[cfg(not(target_has_atomic = "64"))]
compile_error!(
    "jqf-resource requires target_has_atomic = \"64\"; the task-grant identity counter needs 64-bit atomics"
);

mod account;
mod allocation;
mod ambient;
mod context;
mod control;
mod counting_alloc;
pub mod diag;
mod error;
mod limits;
pub mod policy;
mod slab;
pub mod spill;
pub mod task;
mod usage;
mod work;

pub use crate::spill::{RunCursorId, RunId, SpillStore};
pub use account::{OutputPermit, RequestAccount};
pub use allocation::{DepthGuard, OwnedDepthGuard};
pub use ambient::{ScopeGuard, cooperative_refusal, install, tripped, warm_ambient_mutex, with_account};
pub use context::{CooperativeError, EnvironmentSnapshot, ResourceContext, StderrSink};
pub use control::{ContinueControl, Control, ControlError, ControlOutcome};
pub use counting_alloc::{CountingAlloc, ceiling_enforced};
pub use error::{ResourceError, ResourceLimit};
pub use limits::ResourceLimits;
pub use usage::{MemoryCategory, MemoryUsage, UsageSnapshot};
pub use work::{WorkAdmission, WorkMeter};

/// Compiles the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
