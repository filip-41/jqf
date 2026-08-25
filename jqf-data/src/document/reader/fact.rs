//! Attached-fact reader.
//!
//! [`FactReader`] walks the document's fact table in bounded batches. See [`super`] for the poll contract.

use crate::{DataError, Document, DocumentFact};
use jqf_resource::ResourceContext;

use super::{BatchLimit, ReaderCompletion, ReaderDemand, ReaderPoll, admitted_items};

/// Active until the fact table is exhausted, then complete only if the terminal progress check admits the transition;
/// any error latches `Failed`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Active,
    Complete,
    Failed,
}

/// Reader over attached facts in document order.
pub struct FactReader<'document, 'source> {
    document: &'document Document<'source>,
    cursor: usize,
    state: State,
}

impl<'document, 'source> FactReader<'document, 'source> {
    pub(crate) fn new(document: &'document Document<'source>) -> Self {
        FactReader {
            document,
            cursor: 0,
            state: State::Active,
        }
    }

    fn completion(&self) -> ReaderCompletion {
        ReaderCompletion::complete(self.document, ReaderDemand::Facts)
    }

    /// Polls one bounded attached-fact batch.
    pub fn poll_batch<'batch>(
        &'batch mut self,
        limit: BatchLimit,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ReaderPoll<FactBatch<'batch, 'source>>, DataError> {
        match self.state {
            State::Complete => {
                if let Err(error) = resources.check_control() {
                    self.state = State::Failed;
                    return Err(error.into());
                }
                return Ok(ReaderPoll::End(self.completion()));
            }
            State::Failed => return Err(DataError::ReaderFailed),
            State::Active => {}
        }
        let total = self.document.facts().len();
        if self.cursor >= total {
            if let Err(error) = resources.check_control() {
                self.state = State::Failed;
                return Err(error.into());
            }
            self.state = State::Complete;
            return Ok(ReaderPoll::End(self.completion()));
        }
        let requested = (total - self.cursor).min(limit.get());
        let admitted = match admitted_items(resources, requested) {
            Ok(Some(admitted)) => admitted,
            Ok(None) => return Ok(ReaderPoll::Pending),
            Err(error) => {
                self.state = State::Failed;
                return Err(error);
            }
        };
        let start = self.cursor;
        let end = start + admitted;
        if let Err(error) = resources.check_control() {
            self.state = State::Failed;
            return Err(error.into());
        }
        self.cursor = end;
        Ok(ReaderPoll::Batch(FactBatch {
            document: self.document,
            range: start..end,
        }))
    }
}

/// One borrowed attached-fact batch.
pub struct FactBatch<'document, 'source> {
    document: &'document Document<'source>,
    range: core::ops::Range<usize>,
}

impl<'document> FactBatch<'document, '_> {
    /// Returns the number of records in this batch.
    #[must_use]
    pub fn len(&self) -> usize {
        self.range.len()
    }

    /// Returns whether this batch contains no records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.range.is_empty()
    }

    /// Iterates facts in document order without allocating.
    pub fn iter(&self) -> impl Iterator<Item = DocumentFact<'document>> + '_ {
        self.range.clone().map(|index| {
            let stored = &self.document.facts()[index];
            let role = self
                .document
                .storage
                .schema()
                .validated_fact_role(stored.role_binding());
            let kind = self
                .document
                .storage
                .schema()
                .validated_fact_kind(stored.kind_binding());
            DocumentFact { stored, role, kind }
        })
    }
}
