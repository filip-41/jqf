//! Borrowed view of one document node.
//!
//! [`ValueView`] borrows the document and names a node. Array, object, scalar, and tag views all start here.

use crate::document::NodeSemantic;
use crate::{
    ArrayView, DataError, Document, DocumentCapability, IntrinsicTagSemantics, NodeId, ObjectView, ScalarView, TagId,
    ValueKind,
};

/// Borrowed view of one document node.
#[derive(Clone, Copy)]
pub struct ValueView<'document, 'source> {
    pub(crate) document: &'document Document<'source>,
    pub(crate) node: NodeId,
}

impl<'document, 'source> ValueView<'document, 'source> {
    pub(crate) const fn new(document: &'document Document<'source>, node: NodeId) -> Self {
        Self { document, node }
    }

    /// The node this view names.
    #[must_use]
    pub const fn node(self) -> NodeId {
        self.node
    }

    /// Category of the payload. Looks through tag wrappers.
    pub fn kind(self) -> Result<ValueKind, DataError> {
        self.document
            .node_record(self.node)?
            .semantic
            .kind()
            .ok_or(DataError::UnrepresentableSemantic)
    }

    /// Intrinsic tag on this node, if the document kept tags.
    ///
    /// The `Err` arm is a CAPABILITY answer, not a per-node one: a document whose coverage lacks
    /// [`DocumentCapability::IntrinsicTags`] refuses with [`DataError::CapabilityUnavailable`] for every node, while
    /// `Ok(None)` means exactly "this node carries no intrinsic tag". A caller that can fall back to the ordinary path
    /// must treat the capability error as control flow — reading it as "no tag" would misanswer every node of a
    /// tag-less document.
    ///
    /// # Errors
    ///
    /// [`DataError::CapabilityUnavailable`] when the document did not keep intrinsic tags; [`DataError`] from
    /// node-record access otherwise.
    pub fn tag(self) -> Result<Option<&'document TagId>, DataError> {
        Ok(self.intrinsic()?.map(crate::IntrinsicTag::tag))
    }

    /// Whether the intrinsic tag is core or non-core.
    pub fn tag_semantics(self) -> Result<Option<IntrinsicTagSemantics>, DataError> {
        Ok(self.intrinsic()?.map(crate::IntrinsicTag::semantics))
    }

    /// Capability-gated resolved intrinsic tag record.
    fn intrinsic(self) -> Result<Option<&'document crate::IntrinsicTag>, DataError> {
        self.document.require_capability(DocumentCapability::IntrinsicTags)?;
        let record = self.document.node_record(self.node)?;
        Ok(self.document.resolve_intrinsic_tag(record.intrinsic_tag))
    }

    /// Whether this node is still a source span, not a built container.
    ///
    /// Ask this before projecting. A span-backed container still reports array or object from [`kind`](Self::kind), but
    /// it has no occurrences to walk.
    pub fn is_container_span(self) -> Result<bool, DataError> {
        Ok(matches!(
            self.document.node_record(self.node)?.semantic,
            NodeSemantic::ContainerSpan { .. }
        ))
    }

    /// Scalar view when this node is a scalar.
    pub fn scalar(self) -> Result<Option<ScalarView<'document>>, DataError> {
        ScalarView::from_semantic(self.document, &self.document.node_record(self.node)?.semantic)
    }

    /// Returns an array view when this node projects as an array.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::UnmaterializedContainerSpan`] for a span-backed array: it carries no occurrences, so a view
    /// over it would read as EMPTY. Failing closed is the only answer that cannot publish wrong bytes; the toucher
    /// materializes the subtree instead.
    pub fn array(self) -> Result<Option<ArrayView<'document, 'source>>, DataError> {
        let record = self.document.node_record(self.node)?;
        match &record.semantic {
            NodeSemantic::Array { .. } => Ok(Some(ArrayView::new(self.document, self.node))),
            NodeSemantic::ContainerSpan { .. } => Err(DataError::UnmaterializedContainerSpan),
            _ => Ok(None),
        }
    }

    /// Returns an object view when this node projects as an object.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::UnmaterializedContainerSpan`] for a span-backed object, for [`array`](Self::array)'s
    /// reason.
    pub fn object(self) -> Result<Option<ObjectView<'document, 'source>>, DataError> {
        let record = self.document.node_record(self.node)?;
        match &record.semantic {
            NodeSemantic::Object { .. } => Ok(Some(ObjectView::new(self.document, self.node))),
            NodeSemantic::ContainerSpan { .. } => Err(DataError::UnmaterializedContainerSpan),
            _ => Ok(None),
        }
    }
}
