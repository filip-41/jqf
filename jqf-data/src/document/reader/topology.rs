//! Topology reader: nodes first, then occurrences.
//!
//! One poll sequence emits every node batch, then every occurrence batch. A failed reader is terminal. See [`super`]
//! for the poll contract.

use core::ops::{ControlFlow, Range};

use jqf_resource::ResourceContext;

use crate::document::DocumentNodeKindId;
use crate::{DataError, Document, IntrinsicTag, LocalOwnerRef, NodeId, OccurrenceId, OccurrenceRoleId, ValueView};

use super::{
    BatchLimit, ReaderCompletion, ReaderDemand, ReaderPoll, UNBOUNDED_READER_REPLENISH, admitted_items,
    unbounded_batch_limit,
};

enum Phase {
    Nodes,
    Occurrences,
    Complete,
    Failed,
}

/// Reader over all logical nodes and ordered occurrences.
pub struct TopologyReader<'document, 'source> {
    document: &'document Document<'source>,
    phase: Phase,
    cursor: usize,
}

impl<'document, 'source> TopologyReader<'document, 'source> {
    pub(crate) const fn new(document: &'document Document<'source>) -> Self {
        Self {
            document,
            phase: Phase::Nodes,
            cursor: 0,
        }
    }

    /// Polls one bounded node or occurrence batch.
    pub fn poll_batch<'batch>(
        &'batch mut self,
        limit: BatchLimit,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ReaderPoll<TopologyBatch<'batch, 'source>>, DataError> {
        loop {
            let total = match self.phase {
                Phase::Nodes => self.document.node_count(),
                Phase::Occurrences => self.document.semantic_relationship_count(),
                Phase::Complete => {
                    if let Err(error) = resources.check_control() {
                        self.phase = Phase::Failed;
                        return Err(error.into());
                    }
                    return Ok(ReaderPoll::End(ReaderCompletion::complete(
                        self.document,
                        ReaderDemand::Topology,
                    )));
                }
                Phase::Failed => return Err(DataError::ReaderFailed),
            };
            if self.cursor >= total {
                if matches!(self.phase, Phase::Nodes) {
                    self.phase = Phase::Occurrences;
                    self.cursor = 0;
                    continue;
                }
                if let Err(error) = resources.check_control() {
                    self.phase = Phase::Failed;
                    return Err(error.into());
                }
                self.phase = Phase::Complete;
                self.cursor = 0;
                return Ok(ReaderPoll::End(ReaderCompletion::complete(
                    self.document,
                    ReaderDemand::Topology,
                )));
            }
            let requested = (total - self.cursor).min(limit.get());
            let admitted = match admitted_items(resources, requested) {
                Ok(Some(admitted)) => admitted,
                Ok(None) => return Ok(ReaderPoll::Pending),
                Err(error) => {
                    self.phase = Phase::Failed;
                    return Err(error);
                }
            };
            let range = self.cursor..self.cursor + admitted;
            if let Err(error) = resources.check_control() {
                self.phase = Phase::Failed;
                return Err(error.into());
            }
            self.cursor += admitted;
            return Ok(ReaderPoll::Batch(match self.phase {
                Phase::Nodes => TopologyBatch::Nodes(NodeBatch {
                    document: self.document,
                    range,
                }),
                Phase::Occurrences => TopologyBatch::Occurrences(OccurrenceBatch {
                    document: self.document,
                    range,
                }),
                Phase::Complete | Phase::Failed => unreachable!(),
            }));
        }
    }

    /// Walk every logical node. Occurrence batches are consumed and ignored.
    /// [`ReaderPoll::Pending`] refills [`UNBOUNDED_READER_REPLENISH`] work credits.
    pub fn drain_nodes<B>(
        &mut self,
        resources: &mut ResourceContext<'_>,
        mut visit: impl FnMut(DocumentNodeView<'_, '_>) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, DataError> {
        let limit = unbounded_batch_limit();
        loop {
            match self.poll_batch(limit, resources)? {
                ReaderPoll::Batch(TopologyBatch::Nodes(nodes)) => {
                    for node in &nodes {
                        if let ControlFlow::Break(stopped) = visit(node?) {
                            return Ok(ControlFlow::Break(stopped));
                        }
                    }
                }
                ReaderPoll::Batch(TopologyBatch::Occurrences(_)) => {}
                ReaderPoll::Pending => {
                    resources.try_begin_next_cooperative_entry(UNBOUNDED_READER_REPLENISH)?;
                }
                ReaderPoll::End(_) => return Ok(ControlFlow::Continue(())),
            }
        }
    }
}

/// One topology batch family.
pub enum TopologyBatch<'document, 'source> {
    /// Logical node batch.
    Nodes(NodeBatch<'document, 'source>),
    /// Ordered occurrence batch.
    Occurrences(OccurrenceBatch<'document, 'source>),
}

/// Borrowed node batch.
pub struct NodeBatch<'document, 'source> {
    document: &'document Document<'source>,
    range: Range<usize>,
}

impl<'document, 'source> NodeBatch<'document, 'source> {
    /// Returns the number of nodes in the batch.
    #[must_use]
    pub fn len(&self) -> usize {
        self.range.len()
    }

    /// Reports whether this node batch is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.range.is_empty()
    }

    /// Iterates logical node views.
    #[must_use]
    pub fn iter(&self) -> NodeIter<'document, 'source> {
        NodeIter {
            document: self.document,
            range: self.range.clone(),
            intrinsic_tags_available: self
                .document
                .coverage()
                .contains(crate::DocumentCapability::IntrinsicTags),
        }
    }
}

impl<'document, 'source> IntoIterator for &NodeBatch<'document, 'source> {
    type Item = Result<DocumentNodeView<'document, 'source>, DataError>;
    type IntoIter = NodeIter<'document, 'source>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Iterator over a node batch.
pub struct NodeIter<'document, 'source> {
    document: &'document Document<'source>,
    range: Range<usize>,
    /// Constant for the document, hoisted out of `next`: the facts builtin and selector index walk every node, and the
    /// capability read is one coverage lookup per batch, not one per node.
    intrinsic_tags_available: bool,
}

impl<'document, 'source> Iterator for NodeIter<'document, 'source> {
    type Item = Result<DocumentNodeView<'document, 'source>, DataError>;

    fn next(&mut self) -> Option<Self::Item> {
        let index = self.range.next()?;
        let Some(id) = NodeId::try_from_index(index) else {
            return Some(Err(DataError::ArithmeticOverflow));
        };
        let record = match self.document.node_record(id) {
            Ok(value) => value,
            Err(error) => return Some(Err(error)),
        };
        Some(Ok(DocumentNodeView {
            id,
            kind: self.document.storage.schema().validated_node_kind(record.kind),
            semantic: record.semantic.kind().map(|_| ValueView::new(self.document, id)),
            intrinsic_tag: self.document.resolve_intrinsic_tag(record.intrinsic_tag),
            intrinsic_tags_available: self.intrinsic_tags_available,
        }))
    }
}

/// Format-neutral logical node view.
#[derive(Clone, Copy)]
pub struct DocumentNodeView<'document, 'source> {
    id: NodeId,
    kind: &'document DocumentNodeKindId,
    semantic: Option<ValueView<'document, 'source>>,
    intrinsic_tag: Option<&'document IntrinsicTag>,
    intrinsic_tags_available: bool,
}

impl<'document, 'source> DocumentNodeView<'document, 'source> {
    /// Returns the node identity.
    #[must_use]
    pub const fn id(self) -> NodeId {
        self.id
    }

    /// Returns the namespaced node kind.
    #[must_use]
    pub const fn kind(self) -> &'document DocumentNodeKindId {
        self.kind
    }

    /// Returns the semantic view.
    #[must_use]
    pub const fn semantic(self) -> Option<ValueView<'document, 'source>> {
        self.semantic
    }

    /// Returns the resolved intrinsic tag, if any.
    ///
    /// # Errors
    ///
    /// Returns `CapabilityUnavailable` when the document retains no intrinsic-tag observations.
    pub fn intrinsic_tag(self) -> Result<Option<&'document IntrinsicTag>, DataError> {
        if self.intrinsic_tags_available {
            Ok(self.intrinsic_tag)
        } else {
            Err(DataError::CapabilityUnavailable {
                capability: crate::DocumentCapability::IntrinsicTags,
            })
        }
    }
}

/// Borrowed occurrence batch.
pub struct OccurrenceBatch<'document, 'source> {
    document: &'document Document<'source>,
    range: Range<usize>,
}

impl<'document, 'source> OccurrenceBatch<'document, 'source> {
    /// Returns the number of occurrences in the batch.
    #[must_use]
    pub fn len(&self) -> usize {
        self.range.len()
    }

    /// Reports whether this occurrence batch is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.range.is_empty()
    }

    /// Iterates ordered occurrence views.
    #[must_use]
    pub fn iter(&self) -> OccurrenceIter<'document, 'source> {
        OccurrenceIter {
            document: self.document,
            range: self.range.clone(),
        }
    }
}

impl<'document, 'source> IntoIterator for &OccurrenceBatch<'document, 'source> {
    type Item = Result<OccurrenceView<'document>, DataError>;
    type IntoIter = OccurrenceIter<'document, 'source>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Iterator over an occurrence batch.
pub struct OccurrenceIter<'document, 'source> {
    document: &'document Document<'source>,
    range: Range<usize>,
}

impl<'document> Iterator for OccurrenceIter<'document, '_> {
    type Item = Result<OccurrenceView<'document>, DataError>;

    fn next(&mut self) -> Option<Self::Item> {
        let index = self.range.next()?;
        let Some(id) = OccurrenceId::try_from_index(index) else {
            return Some(Err(DataError::ArithmeticOverflow));
        };
        let record = match self.document.occurrence_record(id) {
            Ok(value) => value,
            Err(error) => return Some(Err(error)),
        };
        Some(Ok(OccurrenceView {
            id,
            owner: record.owner,
            role: self.document.storage.schema().validated_occurrence_role(record.role),
            position: u64::from(record.position),
            key_text: record
                .key
                .as_ref()
                .and_then(|key| self.document.occurrence_key_text(key)),
            target: record.target,
        }))
    }
}

/// Format-neutral ordered topology occurrence.
#[derive(Clone, Copy)]
pub struct OccurrenceView<'document> {
    id: OccurrenceId,
    owner: LocalOwnerRef,
    role: &'document OccurrenceRoleId,
    position: u64,
    key_text: Option<&'document str>,
    target: NodeId,
}

impl<'document> OccurrenceView<'document> {
    /// Returns the occurrence identity.
    #[must_use]
    pub const fn id(self) -> OccurrenceId {
        self.id
    }

    /// Returns the local owner.
    #[must_use]
    pub const fn owner(self) -> LocalOwnerRef {
        self.owner
    }

    /// Returns the ordering role.
    #[must_use]
    pub const fn role(self) -> &'document OccurrenceRoleId {
        self.role
    }

    /// Returns the dense zero-based role position.
    #[must_use]
    pub const fn position(self) -> u64 {
        self.position
    }

    /// Returns resolved UTF-8 key text, including source-backed and stored keys.
    #[must_use]
    pub const fn key_text(self) -> Option<&'document str> {
        self.key_text
    }

    /// Returns the target node.
    #[must_use]
    pub const fn target(self) -> NodeId {
        self.target
    }
}
