//! Cancellation, deadline, and "the process is too big".
//!
//! The host implements [`Control`] and decides how those are measured. This crate just carries the answer. Memory can
//! mean an allocator estimate, an OS RSS read, or anything else the host can see.

/// What one [`Control::check`] said.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlOutcome {
    /// Keep going.
    Continue,
    /// The host cancelled the request.
    Cancelled,
    /// The deadline passed.
    DeadlineExceeded,
    /// The process is over the host's physical-memory ceiling.
    MemoryExceeded,
}

/// A stop: cancelled, past the deadline, or over the memory ceiling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlError {
    /// The host cancelled the request.
    Cancelled,
    /// The deadline passed.
    DeadlineExceeded,
    /// The process is over the host's physical-memory ceiling. Not catchable, same as the other two.
    MemoryExceeded,
}

impl core::fmt::Display for ControlError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(match self {
            Self::Cancelled => "request cancelled",
            Self::DeadlineExceeded => "request deadline exceeded",
            Self::MemoryExceeded => "physical memory ceiling exceeded",
        })
    }
}

impl core::error::Error for ControlError {}

/// Ask the host whether the request may keep running.
pub trait Control {
    /// `Continue`, or one of the three stops.
    fn check(&self) -> ControlOutcome;
}

/// A [`Control`] that always says continue. Useful in tests and examples.
#[derive(Clone, Copy, Debug, Default)]
pub struct ContinueControl;

impl Control for ContinueControl {
    fn check(&self) -> ControlOutcome {
        ControlOutcome::Continue
    }
}
