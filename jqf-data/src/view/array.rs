//! Borrowed view of one array node.
//!
//! [`ArrayView`] reads items in order without copying.
//!
//! A view is minted only for a node whose record the caller already resolved (the kind probe behind
//! [`crate::ValueView`] or a checked constructor), so the projection reads here are infallible: a stale or foreign id
//! cannot reach them. [`ArrayView::get`] answers `Option` — an out-of-range index is an ordinary miss, never a signal
//! about the document. That is the same lookup contract as [`ObjectView`](crate::ObjectView)'s keyed `get`: both views
//! answer `None` for a miss, and neither carries an error channel on the single-item read.

use crate::{Document, NodeId, ValueView};

/// Borrowed view of one array node's items.
#[derive(Clone, Copy)]
pub struct ArrayView<'document, 'source> {
    document: &'document Document<'source>,
    node: NodeId,
}

impl<'document, 'source> ArrayView<'document, 'source> {
    pub(crate) const fn new(document: &'document Document<'source>, node: NodeId) -> Self {
        Self { document, node }
    }

    /// Returns the number of semantic array items.
    #[must_use]
    pub fn len(self) -> usize {
        self.items().len()
    }

    /// Reports whether the array is empty.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.items().is_empty()
    }

    /// Returns one item by semantic position.
    #[must_use]
    pub fn get(self, index: usize) -> Option<ValueView<'document, 'source>> {
        self.items().get(index).map(|node| ValueView::new(self.document, node))
    }

    /// Iterates semantic items in topology order.
    #[must_use]
    pub fn iter(self) -> ArrayIter<'document, 'source> {
        ArrayIter {
            document: self.document,
            items: self.items(),
            cursor: 0,
        }
    }

    fn items(self) -> crate::document::ArrayItems<'document> {
        self.document.array_projection(self.node)
    }
}

/// Iterator over borrowed array items.
pub struct ArrayIter<'document, 'source> {
    document: &'document Document<'source>,
    items: crate::document::ArrayItems<'document>,
    cursor: usize,
}

impl<'document, 'source> Iterator for ArrayIter<'document, 'source> {
    type Item = ValueView<'document, 'source>;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.items.get(self.cursor)?;
        self.cursor += 1;
        Some(ValueView::new(self.document, node))
    }
}
