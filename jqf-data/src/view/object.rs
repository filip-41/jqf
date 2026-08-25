//! Borrowed view of one object node.
//!
//! Duplicate keys keep the first position and the last value.
//!
//! A view is minted only for a node whose record the caller already resolved, so a stale or foreign id cannot reach its
//! reads (the same law [`crate::ArrayView`] states). [`ObjectView::get`] therefore answers plain `Option`, exactly like
//! the array view's positional lookup and like the owned [`crate::Value`] twins: `None` is a missing key, never a
//! document fault. The lookup still resolves through the projection index's checked slices; their corruption arms are
//! unreachable for a minted view over an immutable document and collapse to `None` rather than carrying an error
//! channel every caller folded into an internal contract anyway. The reads that DO surface [`DataError`] here —
//! [`ObjectView::get_index`] and [`ObjectIter`] — resolve key TEXT, whose reconstruction failure is reported, not
//! swallowed. The plain-`usize` lengths read the winner table directly and are infallible for the same reason the array
//! view's are.

use crate::{DataError, Document, NodeId, ValueView};

/// Borrowed view of one object's unique keys.
#[derive(Clone, Copy)]
pub struct ObjectView<'document, 'source> {
    document: &'document Document<'source>,
    node: NodeId,
}

impl<'document, 'source> ObjectView<'document, 'source> {
    pub(crate) const fn new(document: &'document Document<'source>, node: NodeId) -> Self {
        Self { document, node }
    }

    /// Returns the number of unique semantic keys.
    #[must_use]
    pub fn len(self) -> usize {
        self.entries().len()
    }

    /// Reports whether the semantic object is empty.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.entries().is_empty()
    }

    /// Looks up the final occurrence value for `key`.
    ///
    /// Answers [`None`] when the key is absent — the same contract as the array view's positional
    /// [`ArrayView::get`](crate::ArrayView::get) and the owned [`crate::Value`] lookups. The checked projection slices
    /// underneath are storage-corruption defenses that a minted view cannot trip (documents are immutable after build);
    /// they fold to `None` instead of an error channel.
    #[must_use]
    pub fn get(self, key: &str) -> Option<ValueView<'document, 'source>> {
        let (entries, index) = self.document.object_projection_lookup(self.node).ok()?;
        let position = if entries.len() <= crate::document::SMALL_OBJECT_WINNER_LIMIT {
            // A small object's lookup segment is never sorted (the per-object sort is skipped at finalize); the winners
            // are walked linearly, which is also faster than a random-access binary search at this size — the
            // owned-value side gates at the same threshold.
            let mut found = None;
            for position in 0..entries.len() {
                let entry = entries.get(position)?;
                let stored_key = self.document.object_projection_key(&entry)?;
                if stored_key == key {
                    found = Some(u32::try_from(position).ok()?);
                    break;
                }
            }
            found
        } else {
            crate::index::find_eytzinger(index, |position| {
                let entry = entries.get(position as usize).ok_or(DataError::InvalidDocument)?;
                let stored_key = self
                    .document
                    .object_projection_key(&entry)
                    .ok_or(DataError::InvalidDocument)?;
                Ok::<_, DataError>(stored_key.cmp(key))
            })
            .ok()?
        };
        let entry = entries.get(position? as usize)?;
        Some(ValueView::new(self.document, entry.target))
    }

    /// Returns one semantic entry by first-insertion position.
    pub fn get_index(self, index: usize) -> Result<Option<ObjectEntryView<'document, 'source>>, DataError> {
        let Some(entry) = self.entries().get(index) else {
            return Ok(None);
        };
        let key = self
            .document
            .object_projection_key(&entry)
            .ok_or(DataError::InvalidDocument)?;
        Ok(Some(ObjectEntryView {
            key,
            value: ValueView::new(self.document, entry.target),
        }))
    }

    /// Iterates first-insertion keys with final occurrence values.
    #[must_use]
    pub fn iter(self) -> ObjectIter<'document, 'source> {
        ObjectIter {
            document: self.document,
            entries: self.entries(),
            cursor: 0,
        }
    }

    fn entries(self) -> crate::document::ObjectEntries<'document> {
        self.document.object_projection(self.node)
    }
}

/// One borrowed key/value pair.
#[derive(Clone, Copy)]
pub struct ObjectEntryView<'document, 'source> {
    key: &'document str,
    value: ValueView<'document, 'source>,
}

impl<'document, 'source> ObjectEntryView<'document, 'source> {
    /// Returns the key at its first insertion position.
    #[must_use]
    pub const fn key(self) -> &'document str {
        self.key
    }

    /// Returns the value from the final key occurrence.
    #[must_use]
    pub const fn value(self) -> ValueView<'document, 'source> {
        self.value
    }
}

/// Iterator over unique object entries.
pub struct ObjectIter<'document, 'source> {
    document: &'document crate::Document<'source>,
    entries: crate::document::ObjectEntries<'document>,
    cursor: usize,
}

impl<'document, 'source> Iterator for ObjectIter<'document, 'source> {
    type Item = Result<ObjectEntryView<'document, 'source>, DataError>;

    fn next(&mut self) -> Option<Self::Item> {
        let entry = self.entries.get(self.cursor)?;
        self.cursor += 1;
        let key = self
            .document
            .object_projection_key(&entry)
            .ok_or(DataError::InvalidDocument);
        Some(key.map(|key| ObjectEntryView {
            key,
            value: ValueView::new(self.document, entry.target),
        }))
    }
}
