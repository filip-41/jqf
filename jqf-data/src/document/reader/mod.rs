//! Bounded readers: topology and attached facts.
//!
//! [`TopologyReader`] and [`FactReader`] walk one document in batches. Each poll returns a nonempty batch,
//! [`ReaderPoll::Pending`] when work credits run out, or a [`ReaderCompletion`] at the end. A failed reader is terminal
//! ([`DataError::ReaderFailed`]).

mod fact;
mod topology;

pub use fact::{FactBatch, FactReader};
pub use topology::{
    DocumentNodeView, NodeBatch, NodeIter, OccurrenceBatch, OccurrenceIter, OccurrenceView, TopologyBatch,
    TopologyReader,
};

use crate::{DataError, Document, DocumentCapability};

/// Hard item cap for one reader poll.
///
/// Work admission from `jqf-resource` also caps the batch. There is no caller-chosen byte budget.
pub type BatchLimit = core::num::NonZeroUsize;

/// Which reader produced a [`ReaderCompletion`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReaderDemand {
    /// The topology reader finished.
    Topology,
    /// The attached-fact reader finished.
    Facts,
}

/// Terminal reader evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReaderCompletion {
    document: crate::DocumentId,
    demand: ReaderDemand,
}

impl ReaderCompletion {
    pub(crate) fn complete(document: &Document<'_>, demand: ReaderDemand) -> Self {
        Self {
            document: document.key(),
            demand,
        }
    }

    /// Returns the exact document covered by this completion.
    #[must_use]
    pub const fn document(self) -> crate::DocumentId {
        self.document
    }

    /// Returns the exact normalized demand covered by this completion.
    #[must_use]
    pub const fn demand(self) -> ReaderDemand {
        self.demand
    }
}

/// One cooperative reader poll result.
#[derive(Debug)]
pub enum ReaderPoll<Batch> {
    /// One nonempty bounded batch.
    Batch(Batch),
    /// The shared work slice was exhausted before beginning another item.
    Pending,
    /// Terminal completion and its document-and-demand-bound evidence.
    End(ReaderCompletion),
}

impl<'source> Document<'source> {
    /// Opens a complete topology reader.
    pub fn topology_reader(
        &self,
        resources: &mut jqf_resource::ResourceContext<'_>,
    ) -> Result<TopologyReader<'_, 'source>, DataError> {
        resources.check_control()?;
        self.require_capability(DocumentCapability::Topology)?;
        Ok(TopologyReader::new(self))
    }

    /// Opens an attached-fact reader.
    pub fn fact_reader(
        &self,
        resources: &mut jqf_resource::ResourceContext<'_>,
    ) -> Result<FactReader<'_, 'source>, DataError> {
        resources.check_control()?;
        self.require_capability(DocumentCapability::AttachedFacts)?;
        Ok(FactReader::new(self))
    }
}

/// Admits up to `requested` items against the shared work slice, looping while partial grants keep coming; returns
/// `None` when the first grant is refused before any item, and a partial count once a later refusal or a zero grant
/// ends the loop.
pub(crate) fn admitted_items(
    resources: &mut jqf_resource::ResourceContext<'_>,
    requested: usize,
) -> Result<Option<usize>, DataError> {
    let mut admitted = 0_usize;
    while admitted < requested {
        match resources.admit_work_bytes(requested - admitted)? {
            // A zero or refused grant before any item would hand the reader an empty batch its consumer loops on
            // forever; only a partial grant may end the loop.
            jqf_resource::WorkAdmission::Granted(0) | jqf_resource::WorkAdmission::Pending if admitted == 0 => {
                return Ok(None);
            }
            jqf_resource::WorkAdmission::Granted(0) | jqf_resource::WorkAdmission::Pending => break,
            jqf_resource::WorkAdmission::Granted(items) => {
                admitted = admitted.checked_add(items).ok_or(DataError::ArithmeticOverflow)?;
            }
        }
    }
    Ok(Some(admitted))
}
