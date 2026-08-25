//! Where a sort writes overflow runs when they no longer fit in memory.
//!
//! This crate names the operations. The host owns the files. You hand over a whole run as bytes and read entries back
//! one at a time. No path or file type crosses the boundary. After a failed write, stay in memory, or fail the request
//! if bytes already spilled cannot be taken back.

use std::vec::Vec;

/// One run on the host, numbered by the store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunId(pub u64);

/// One open sequential read cursor over a run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunCursorId(pub u64);

/// Disk store for overflow sort runs.
///
/// Methods take `&self` because the context holds `&dyn SpillStore`. Use interior mutability on your side.
///
/// # Errors
///
/// Return [`crate::ResourceError::HostFailure`] when the host cannot do it (disk full, missing temp dir, vanished run).
/// After a failed write or read, either stay in memory or fail the request if bytes already spilled cannot be taken
/// back.
pub trait SpillStore {
    /// Creates a new run and returns its handle. The run is EMPTY until [`Self::write_run`] fills it.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ResourceError::HostFailure`] when the host cannot create the file.
    fn create_run(&self) -> Result<RunId, crate::ResourceError>;

    /// Replaces the run's contents with `bytes` (the caller's full encoded run). A run may be written exactly once.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ResourceError::HostFailure`] when the write fails.
    fn write_run(&self, id: RunId, bytes: &[u8]) -> Result<(), crate::ResourceError>;

    /// Opens a run for sequential reading.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ResourceError::HostFailure`] when the run cannot be opened.
    fn open_run(&self, id: RunId) -> Result<RunCursorId, crate::ResourceError>;

    /// Reads the NEXT entry of the cursor's run into `out` (appended), and returns its position when one was read.
    /// `None` ends the run.
    ///
    /// The caller owns `out`: the entry bytes are decoded from it with the length prefix the writer stamped, so the
    /// buffer never needs to hold more than one entry.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ResourceError::HostFailure`] when the read fails.
    fn read_next(&self, cursor: RunCursorId, out: &mut Vec<u8>) -> Result<Option<u64>, crate::ResourceError>;

    /// Releases a run's host resources. Idempotent; a released run may not be opened again.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ResourceError::HostFailure`] when the host cannot release the run.
    fn delete_run(&self, id: RunId) -> Result<(), crate::ResourceError>;
}
