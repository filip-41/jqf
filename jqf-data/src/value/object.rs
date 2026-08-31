//! Objects: unique UTF-8 keys, first-insertion order.
//!
//! Short keys live inline. Longer keys share text. Lookup uses a compact table or an incremental tree.

mod index;
mod key;

use alloc::vec::Vec;
use core::slice;

use super::shared::Shared;
use super::{Value, ValueAllocationError};
use index::{ObjectIncrementalIndex, ObjectLookupIndex, ObjectStorageIndex, build_unique_incremental_index};
pub use key::ObjectKey;

pub(crate) const LINEAR_DEDUP_THRESHOLD: usize = 16;
/// One key/value pair.
#[derive(Debug)]
pub struct ObjectEntry {
    key: ObjectKey,
    value: Value,
}

impl ObjectEntry {
    /// The key as text.
    #[must_use]
    pub fn key(&self) -> &str {
        self.key.as_str()
    }

    /// Another handle on this entry's key, for reuse in another object.
    #[must_use]
    pub fn clone_key(&self) -> ObjectKey {
        self.key.clone_shared()
    }

    /// Charged copy of this entry's key as a string value. See [`ObjectKey::try_to_value_string`].
    pub fn try_to_value_string(&self) -> Result<Value, ValueAllocationError> {
        self.key.try_to_value_string()
    }

    /// The value.
    #[must_use]
    pub const fn value(&self) -> &Value {
        &self.value
    }

    /// Mutable access to the value.
    #[must_use]
    pub(crate) fn value_mut(&mut self) -> &mut Value {
        &mut self.value
    }
}

/// Unique UTF-8 keys, first-insertion order.
///
/// The entry table lives in one allocation. Clone is a refcount bump.
#[derive(Clone, Debug)]
pub struct Object(Shared<ObjectStorage>);

#[derive(Debug, Default)]
struct ObjectStorage {
    entries: Vec<ObjectEntry>,
    index: Option<ObjectStorageIndex>,
}

impl Object {
    /// Empty object. Fails if the allocator refuses.
    pub fn try_new() -> Result<Self, ValueAllocationError> {
        try_shared_storage(ObjectStorage::default()).map(Self)
    }
    /// How many unique keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.entries.len()
    }

    /// True when there are no keys.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.entries.is_empty()
    }

    /// Value for `key`, if present.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.key_position(key)
            .and_then(|position| self.0.entries.get(position))
            .map(ObjectEntry::value)
    }

    /// Insertion index of `key`.
    ///
    /// Use this when you need the index before a mutable borrow: a write may copy the table, and the index still points
    /// at the same entry.
    #[must_use]
    pub fn key_position(&self, key: &str) -> Option<usize> {
        if let Some(index) = &self.0.index {
            return index.get(&self.0.entries, key);
        }
        self.0.entries.iter().position(|entry| entry.key.as_str() == key)
    }

    /// Entry at insertion index `index`.
    #[must_use]
    pub fn get_index(&self, index: usize) -> Option<&ObjectEntry> {
        self.0.entries.get(index)
    }

    /// Mutable access to the value at insertion index `index`.
    pub fn try_get_index_mut(&mut self, index: usize) -> Result<Option<&mut Value>, ValueAllocationError> {
        if index >= self.0.entries.len() {
            return Ok(None);
        }
        Ok(self
            .try_storage_mut(0)?
            .entries
            .get_mut(index)
            .map(ObjectEntry::value_mut))
    }

    /// Mutable access to the value for `key`.
    ///
    /// Copies first if another handle still sees this table. Missing key returns `Ok(None)` without copying.
    pub fn try_get_mut(&mut self, key: &str) -> Result<Option<&mut Value>, ValueAllocationError> {
        let Some(index) = self.key_position(key) else {
            return Ok(None);
        };
        self.try_get_index_mut(index)
    }

    /// Insert `key` only if it is new. Returns `false` if it already exists.
    pub fn try_insert_unique(&mut self, key: ObjectKey, value: Value) -> Result<bool, ValueAllocationError> {
        let storage = self.try_storage_mut(1)?;
        if let Some(index) = &mut storage.index {
            if index
                .try_occupy_or_insert(&storage.entries, key.as_str(), storage.entries.len())
                .map_err(|_| ValueAllocationError)?
                .is_some()
            {
                return Ok(false);
            }
        } else if storage.entries.iter().any(|entry| entry.key.as_str() == key.as_str()) {
            return Ok(false);
        } else if storage.entries.len() >= LINEAR_DEDUP_THRESHOLD {
            let mut index = build_unique_incremental_index(&storage.entries).map_err(|_| ValueAllocationError)?;
            index
                .try_insert(&storage.entries, key.as_str(), storage.entries.len())
                .map_err(|_| ValueAllocationError)?;
            storage.index = Some(ObjectStorageIndex::Incremental(index));
        }
        storage.entries.push(ObjectEntry { key, value });
        Ok(true)
    }

    /// Insert `key`, or replace its value if it already exists.
    ///
    /// An existing key keeps its first-insertion position. Returns the previous value when replacing.
    pub fn try_insert_or_replace(
        &mut self,
        key: ObjectKey,
        value: Value,
    ) -> Result<Option<Value>, ValueAllocationError> {
        if let Some(index) = self.key_position(key.as_str()) {
            let slot = self.try_get_index_mut(index)?.ok_or(ValueAllocationError)?;
            return Ok(Some(core::mem::replace(slot, value)));
        }
        if !self.try_insert_unique(key, value)? {
            return Err(ValueAllocationError);
        }
        Ok(None)
    }

    /// Entries in first-insertion order.
    pub fn iter(&self) -> slice::Iter<'_, ObjectEntry> {
        self.0.entries.iter()
    }

    /// Another handle on the same entry table.
    #[must_use]
    pub fn clone_shared(&self) -> Self {
        Self(self.0.clone_shared())
    }
}

impl Object {
    /// An allocation-identity key for this entry table.
    ///
    /// Two LIVE handles with equal keys alias one entry table — exactly what [`Self::shares_storage_with`] answers
    /// between them. A key stays valid only while some handle still holds the table: after the last handle drops, a
    /// fresh table may reuse the address and a stale key names nothing. Over a borrowed value tree every referenced
    /// table is pinned by its live refcount for the whole walk, so keys taken during one walk stay exact across it.
    #[must_use]
    pub fn allocation_key(&self) -> usize {
        self.0.allocation_key()
    }

    /// Whether these two handles name the same entry table.
    #[must_use]
    pub fn shares_storage_with(&self, other: &Self) -> bool {
        self.0.shares_allocation_with(&other.0)
    }

    /// Detaches or reuses the entry table for a mutation that adds no entry.
    fn try_storage_mut(&mut self, additional: usize) -> Result<&mut ObjectStorage, ValueAllocationError> {
        if self.0.is_unique() {
            let storage = self.0.borrow_owned()?;
            storage
                .entries
                .try_reserve(additional)
                .map_err(|_| ValueAllocationError)?;
            return Ok(storage);
        }
        self.try_detach(try_clone_storage(&self.0, additional)?)
    }

    fn try_detach(&mut self, storage: ObjectStorage) -> Result<&mut ObjectStorage, ValueAllocationError> {
        self.0 = try_shared_storage(storage)?;
        self.0.payload_mut().ok_or(ValueAllocationError)
    }
}

fn try_shared_storage(storage: ObjectStorage) -> Result<Shared<ObjectStorage>, ValueAllocationError> {
    Shared::try_new(storage)
}

impl<'a> IntoIterator for &'a Object {
    type Item = &'a ObjectEntry;
    type IntoIter = slice::Iter<'a, ObjectEntry>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.entries.iter()
    }
}

/// Builds an object: first key position, last value.
///
/// Collect occurrences, then finish. Small objects walk the list. Wider ones build a sorted index — no hash table, so
/// a hostile key set cannot blow up the work.
#[derive(Debug, Default)]
pub struct ObjectBuilder {
    entries: Vec<ObjectEntry>,
    index: Option<ObjectIncrementalIndex>,
}

impl ObjectBuilder {
    /// Empty builder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
            index: None,
        }
    }

    /// Empty builder with room for `capacity` occurrences.
    pub fn try_with_capacity(capacity: usize) -> Result<Self, ValueAllocationError> {
        let mut entries = Vec::new();
        entries.try_reserve_exact(capacity).map_err(|_| ValueAllocationError)?;
        Ok(Self { entries, index: None })
    }

    /// Append a key/value pair. Caller must keep keys unique and in first insertion order — this does not scan for
    /// duplicates.
    ///
    /// If [`Self::try_insert_or_replace`] already built an index, this falls back to that insert so a blind append
    /// cannot hide a duplicate.
    pub fn try_insert_last(&mut self, key: ObjectKey, value: Value) -> Result<(), ValueAllocationError> {
        if self.index.is_some() {
            return self.try_insert_or_replace(key, value);
        }
        self.entries.try_reserve(1).map_err(|_| ValueAllocationError)?;
        self.entries.push(ObjectEntry { key, value });
        Ok(())
    }

    /// Replace the value at `position`.
    ///
    /// `position` must come from this builder's own insertion order (a `key_position` answer). `None` names no such
    /// position — a caller bug, not an allocation failure. No duplicate scan.
    pub fn replace_value_at(&mut self, position: usize, value: Value) -> Option<()> {
        let entry = self.entries.get_mut(position)?;
        entry.value = value;
        Some(())
    }

    /// Insert `key`, or replace its value if it already exists.
    pub fn try_insert_or_replace(&mut self, key: ObjectKey, value: Value) -> Result<(), ValueAllocationError> {
        if let Some(index) = &mut self.index {
            self.entries.try_reserve(1).map_err(|_| ValueAllocationError)?;
            let position = self.entries.len();
            if let Some(existing) = index
                .try_occupy_or_insert(&self.entries, key.as_str(), position)
                .map_err(|_| ValueAllocationError)?
            {
                self.entries[existing].value = value;
                return Ok(());
            }
            self.entries.push(ObjectEntry { key, value });
            return Ok(());
        }
        if let Some(position) = self.entries.iter().position(|entry| entry.key.as_str() == key.as_str()) {
            self.entries[position].value = value;
            return Ok(());
        }
        self.entries.try_reserve(1).map_err(|_| ValueAllocationError)?;
        let position = self.entries.len();
        if position < LINEAR_DEDUP_THRESHOLD {
            self.entries.push(ObjectEntry { key, value });
            return Ok(());
        }
        let mut index = build_unique_incremental_index(&self.entries).map_err(|_| ValueAllocationError)?;
        index
            .try_insert(&self.entries, key.as_str(), position)
            .map_err(|_| ValueAllocationError)?;
        self.entries.push(ObjectEntry { key, value });
        self.index = Some(index);
        Ok(())
    }

    /// Finish when the caller already proved keys unique.
    ///
    /// `positions` is the caller's key order. Small objects keep no index; walking the list is cheaper than a second
    /// array.
    pub(crate) fn try_finish_unique_with_lookup(self, positions: Vec<usize>) -> Result<Object, ValueAllocationError> {
        debug_assert!(
            keys_are_unique(&self.entries),
            "try_finish_unique_with_lookup requires unique keys"
        );
        if positions.len() != self.entries.len() {
            // A key order that does not cover every entry is a caller bug. Debug builds assert; release falls back to
            // the crate's single fallible signal rather than publishing a half-built object.
            debug_assert!(false, "key order does not cover every entry");
            return Err(ValueAllocationError);
        }
        try_shared_storage(ObjectStorage {
            index: (positions.len() > LINEAR_DEDUP_THRESHOLD)
                .then_some(ObjectStorageIndex::Lookup(ObjectLookupIndex { positions })),
            entries: self.entries,
        })
        .map(Object)
    }

    /// Finish the object. First key position, last value.
    pub fn try_finish(self) -> Result<Object, ValueAllocationError> {
        if let Some(index) = self.index {
            return try_shared_storage(ObjectStorage {
                entries: self.entries,
                index: Some(ObjectStorageIndex::Incremental(index)),
            })
            .map(Object);
        }
        if self.entries.len() <= LINEAR_DEDUP_THRESHOLD {
            return finish_linear(self.entries);
        }
        finish_sorted(self.entries)
    }

    /// Finish when keys are already unique and last-value-wins is already applied. Skips the duplicate scan.
    ///
    /// Public builders use [`Self::try_finish`]. This path is for codecs that already proved uniqueness; debug builds
    /// assert it. Duplicate keys on this path are a caller bug.
    #[doc(hidden)]
    pub fn try_finish_unique(self) -> Result<Object, ValueAllocationError> {
        debug_assert!(keys_are_unique(&self.entries), "try_finish_unique requires unique keys");
        let index = if self.entries.len() > LINEAR_DEDUP_THRESHOLD {
            Some(ObjectStorageIndex::Lookup(
                ObjectLookupIndex::try_from_unique_entries(&self.entries, 0).map_err(|_| ValueAllocationError)?,
            ))
        } else {
            None
        };
        try_shared_storage(ObjectStorage {
            entries: self.entries,
            index,
        })
        .map(Object)
    }
}

fn keys_are_unique(entries: &[ObjectEntry]) -> bool {
    entries.iter().enumerate().all(|(index, entry)| {
        entries[..index]
            .iter()
            .all(|other| other.key.as_str() != entry.key.as_str())
    })
}

fn finish_linear(mut entries: Vec<ObjectEntry>) -> Result<Object, ValueAllocationError> {
    let mut first = 0;
    while first < entries.len() {
        let mut last = first;
        for candidate in first + 1..entries.len() {
            if entries[candidate].key.as_str() == entries[first].key.as_str() {
                last = candidate;
            }
        }
        if last != first {
            let (before_last, from_last) = entries.split_at_mut(last);
            core::mem::swap(&mut before_last[first].value, &mut from_last[0].value);
        }
        let mut candidate = first + 1;
        while candidate < entries.len() {
            if entries[candidate].key.as_str() == entries[first].key.as_str() {
                entries.remove(candidate);
            } else {
                candidate += 1;
            }
        }
        first += 1;
    }
    try_shared_storage(ObjectStorage { entries, index: None }).map(Object)
}

fn finish_sorted(mut entries: Vec<ObjectEntry>) -> Result<Object, ValueAllocationError> {
    let mut indices = Vec::new();
    indices
        .try_reserve_exact(entries.len())
        .map_err(|_| ValueAllocationError)?;
    indices.extend(0..entries.len());
    indices.sort_unstable_by(|left, right| {
        entries[*left]
            .key
            .as_str()
            .cmp(entries[*right].key.as_str())
            .then_with(|| left.cmp(right))
    });

    let mut cursor = 0;
    let mut groups = 0;
    while cursor < indices.len() {
        let first = indices[cursor];
        let mut last = first;
        cursor += 1;
        while cursor < indices.len() && entries[indices[cursor]].key.as_str() == entries[first].key.as_str() {
            last = indices[cursor];
            cursor += 1;
        }
        if first != last {
            let (before_last, from_last) = entries.split_at_mut(last);
            core::mem::swap(&mut before_last[first].value, &mut from_last[0].value);
        }
        indices[groups] = first;
        groups += 1;
    }
    indices.truncate(groups);
    indices.sort_unstable();

    let mut source_index = 0;
    let mut next_kept = 0;
    entries.retain(|_| {
        let keep = indices.get(next_kept) == Some(&source_index);
        source_index += 1;
        next_kept += usize::from(keep);
        keep
    });
    let index = if entries.len() <= LINEAR_DEDUP_THRESHOLD {
        None
    } else {
        Some(ObjectStorageIndex::Lookup(
            ObjectLookupIndex::try_from_unique_entries(&entries, 0).map_err(|_| ValueAllocationError)?,
        ))
    };
    try_shared_storage(ObjectStorage { entries, index }).map(Object)
}

fn try_clone_storage(storage: &ObjectStorage, additional: usize) -> Result<ObjectStorage, ValueAllocationError> {
    let entries = try_clone_entries(&storage.entries, additional)?;
    let index = try_clone_storage_index(storage.index.as_ref(), additional)?;
    Ok(ObjectStorage { entries, index })
}

fn try_clone_entries(source: &[ObjectEntry], additional: usize) -> Result<Vec<ObjectEntry>, ValueAllocationError> {
    let mut entries = Vec::new();
    let capacity = source.len().checked_add(additional).ok_or(ValueAllocationError)?;
    entries.try_reserve_exact(capacity).map_err(|_| ValueAllocationError)?;
    for entry in source {
        entries.push(ObjectEntry {
            // The detached table is a SECOND holder of the same key text, and it pays the same per-entry allowance the
            // source entry pays, so the key allocation needs no copy and no fresh reservation.
            key: entry.key.clone_shared(),
            value: entry.value.clone(),
        });
    }
    Ok(entries)
}

fn try_clone_storage_index(
    index: Option<&ObjectStorageIndex>,
    additional: usize,
) -> Result<Option<ObjectStorageIndex>, ValueAllocationError> {
    let Some(index) = index else {
        return Ok(None);
    };
    match index {
        // A detach clones the entries in order, so every Eytzinger position stays valid — keep the compact table and
        // let the first real insertion convert it to the incremental tree (`try_insert` does that lazily). Building the
        // tree here would pay O(n) AVL nodes for a value-only write.
        ObjectStorageIndex::Lookup(index) => {
            let mut positions = Vec::new();
            positions
                .try_reserve_exact(index.positions.len().saturating_add(additional))
                .map_err(|_| ValueAllocationError)?;
            positions.extend_from_slice(&index.positions);
            Ok(Some(ObjectStorageIndex::Lookup(ObjectLookupIndex { positions })))
        }
        ObjectStorageIndex::Incremental(index) => {
            let mut nodes = Vec::new();
            nodes
                .try_reserve_exact(index.nodes.len().saturating_add(additional))
                .map_err(|_| ValueAllocationError)?;
            nodes.extend_from_slice(&index.nodes);
            Ok(Some(ObjectStorageIndex::Incremental(ObjectIncrementalIndex {
                nodes,
                root: index.root,
            })))
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;

    use super::*;

    fn key(text: &str) -> ObjectKey {
        ObjectKey::try_from_str(text).expect("fixture key")
    }

    fn finish_unique(count: usize) -> Object {
        let mut builder = ObjectBuilder::try_with_capacity(count).expect("fixture reserves");
        for index in (0..count).rev() {
            builder
                .try_insert_last(key(&format!("key-{index:02}")), Value::Bool(index & 1 == 0))
                .expect("fixture inserts");
        }
        builder.try_finish().expect("fixture finishes")
    }

    /// The append-without-scanning insert may not smuggle a duplicate key past an incremental index that was already
    /// built: the index cannot see a blind append, so the duplicate would survive the index-retaining finish and the
    /// object would answer with two entries under one key.
    #[test]
    fn appending_onto_an_indexed_builder_still_answers_first_position_last_value() {
        let mut builder = ObjectBuilder::new();
        for index in 0..=LINEAR_DEDUP_THRESHOLD {
            builder
                .try_insert_or_replace(key(&format!("key-{index:02}")), Value::Bool(false))
                .expect("fixture inserts");
        }
        assert!(builder.index.is_some(), "fixture must build the index");

        builder
            .try_insert_last(key("key-00"), Value::Bool(true))
            .expect("append onto the indexed builder");

        let object = builder.try_finish().expect("fixture finishes");
        assert_eq!(object.len(), LINEAR_DEDUP_THRESHOLD + 1);
        assert!(matches!(object.get("key-00"), Some(Value::Bool(true))));
        assert_eq!(
            object
                .into_iter()
                .filter(|entry| entry.key.as_str() == "key-00")
                .count(),
            1
        );
    }

    #[test]
    fn finished_lookup_index_starts_above_the_linear_threshold() {
        let linear = finish_unique(LINEAR_DEDUP_THRESHOLD);
        assert!(linear.0.index.is_none());

        let indexed = finish_unique(LINEAR_DEDUP_THRESHOLD + 1);
        let Some(ObjectStorageIndex::Lookup(index)) = &indexed.0.index else {
            panic!("ordinary wide object must retain the compact lookup index");
        };
        assert_eq!(index.positions.len(), indexed.len());
        let mut positions = index.positions.clone();
        positions.sort_unstable();
        assert_eq!(positions, (0..indexed.len()).collect::<Vec<_>>());
        assert!(matches!(indexed.get("key-00"), Some(Value::Bool(true))));
    }

    #[test]
    fn public_finish_collapses_duplicate_keys_to_first_position_last_value() {
        let mut builder = ObjectBuilder::new();
        builder
            .try_insert_last(key("a"), Value::Bool(false))
            .expect("first insert");
        builder
            .try_insert_last(key("a"), Value::Bool(true))
            .expect("duplicate append");
        builder.try_insert_last(key("b"), Value::Null).expect("second key");
        let object = builder.try_finish().expect("public finish last-value-wins");
        assert_eq!(object.len(), 2);
        assert!(matches!(object.get("a"), Some(Value::Bool(true))));
        assert_eq!(object.into_iter().filter(|entry| entry.key.as_str() == "a").count(), 1);
    }

    #[test]
    fn unique_finish_skips_the_dedup_scan_and_builds_the_same_shapes() {
        // Below the threshold, the unique finish must produce the same index-free shape as the ordinary finish.
        let mut small = ObjectBuilder::try_with_capacity(4).expect("fixture reserves");
        small
            .try_insert_last(key("a"), Value::Bool(true))
            .expect("fixture inserts");
        small
            .try_insert_last(key("b"), Value::Bool(false))
            .expect("fixture inserts");
        let small = small.try_finish_unique().expect("unique finish");
        assert_eq!(small.len(), 2);
        assert!(small.0.index.is_none());

        // Above the threshold, the unique finish retains the sorted lookup exactly like the ordinary finish's tail.
        let mut wide = ObjectBuilder::try_with_capacity(LINEAR_DEDUP_THRESHOLD + 1).expect("fixture reserves");
        for index in (0..=LINEAR_DEDUP_THRESHOLD).rev() {
            wide.try_insert_last(key(&format!("key-{index:02}")), Value::Bool(index & 1 == 0))
                .expect("fixture inserts");
        }
        let wide = wide.try_finish_unique().expect("unique finish");
        assert_eq!(wide.len(), LINEAR_DEDUP_THRESHOLD + 1);
        assert!(matches!(&wide.0.index, Some(ObjectStorageIndex::Lookup(_))));
    }

    #[test]
    fn replace_value_at_rewrites_one_position_without_scanning() {
        let mut builder = ObjectBuilder::try_with_capacity(2).expect("fixture reserves");
        builder
            .try_insert_last(key("a"), Value::Bool(true))
            .expect("fixture inserts");
        builder
            .try_insert_last(key("b"), Value::Bool(false))
            .expect("fixture inserts");
        builder.replace_value_at(0, Value::Null).expect("replace in range");
        let object = builder.try_finish_unique().expect("unique finish");
        assert!(matches!(object.get("a"), Some(Value::Null)));
        assert!(matches!(object.get("b"), Some(Value::Bool(false))));
        assert!(
            ObjectBuilder::try_with_capacity(1)
                .expect("fixture reserves")
                .replace_value_at(3, Value::Null)
                .is_none(),
            "out-of-range replacement must answer None"
        );
    }

    #[test]
    fn eytzinger_lookup_covers_incomplete_tree_sizes_and_misses() {
        for count in [17, 18, 31, 32, 33, 63, 64, 65, 127] {
            let object = finish_unique(count);
            for index in 0..count {
                assert!(object.get(&format!("key-{index:02}")).is_some());
            }
            for missing in ["", "key--1", "key-999", "zzzz"] {
                assert!(object.get(missing).is_none());
            }
        }
    }

    #[test]
    fn incremental_and_cow_mutations_keep_indexes_valid() {
        let mut builder = ObjectBuilder::new();
        for index in 0..LINEAR_DEDUP_THRESHOLD {
            builder
                .try_insert_or_replace(key(&format!("key-{index:02}")), Value::Bool(false))
                .expect("incremental fixture inserts");
        }
        assert!(builder.index.is_none());
        builder
            .try_insert_or_replace(key(&format!("key-{LINEAR_DEDUP_THRESHOLD:02}")), Value::Bool(false))
            .expect("incremental fixture crosses threshold");
        let incremental = builder.try_finish().expect("incremental fixture finishes");
        assert!(matches!(&incremental.0.index, Some(ObjectStorageIndex::Incremental(_))));

        let original = finish_unique(LINEAR_DEDUP_THRESHOLD + 1);
        let mut detached = original.clone();
        *detached
            .try_get_index_mut(0)
            .expect("COW detach succeeds")
            .expect("entry exists") = Value::Null;
        // A detach clones entries IN ORDER, so the compact Eytzinger table stays valid — it is kept, not eagerly
        // rebuilt as the incremental tree; the first real insertion converts it lazily.
        assert!(matches!(&detached.0.index, Some(ObjectStorageIndex::Lookup(_))));
        assert!(matches!(detached.get("key-16"), Some(Value::Null)));
        assert!(matches!(original.get("key-16"), Some(Value::Bool(true))));

        detached
            .try_insert_unique(key("key-99"), Value::Bool(true))
            .expect("indexed insertion succeeds");
        assert!(matches!(detached.get("key-99"), Some(Value::Bool(true))));
    }

    #[test]
    fn try_get_mut_missing_key_does_not_copy() {
        let original = finish_unique(2);
        let mut twin = original.clone();
        assert!(twin.try_get_mut("missing").expect("lookup succeeds").is_none());
        assert!(original.shares_storage_with(&twin));
    }

    #[test]
    fn try_get_mut_detaches_a_shared_table() {
        let original = finish_unique(2);
        let mut twin = original.clone();
        *twin
            .try_get_mut("key-01")
            .expect("lookup succeeds")
            .expect("entry exists") = Value::Null;
        assert!(!original.shares_storage_with(&twin));
        assert!(matches!(twin.get("key-01"), Some(Value::Null)));
        assert!(matches!(original.get("key-01"), Some(Value::Bool(false))));
    }

    #[test]
    fn try_insert_or_replace_keeps_first_position() {
        let mut object = finish_unique(2);
        let previous = object
            .try_insert_or_replace(key("key-01"), Value::Null)
            .expect("replace succeeds");
        assert!(matches!(previous, Some(Value::Bool(false))));
        assert_eq!(object.len(), 2);
        assert_eq!(object.get_index(0).expect("first entry").key(), "key-01");
        assert!(matches!(object.get("key-01"), Some(Value::Null)));

        let previous = object
            .try_insert_or_replace(key("key-99"), Value::Bool(true))
            .expect("insert succeeds");
        assert!(previous.is_none());
        assert_eq!(object.len(), 3);
        assert_eq!(object.get_index(2).expect("appended entry").key(), "key-99");
    }

    #[test]
    fn duplicate_finish_preserves_object_law_around_lookup_threshold() {
        for unique in [LINEAR_DEDUP_THRESHOLD, LINEAR_DEDUP_THRESHOLD + 1] {
            let mut builder = ObjectBuilder::new();
            for index in 0..unique {
                builder
                    .try_insert_last(key(&format!("key-{index:02}")), Value::Bool(false))
                    .expect("fixture inserts first occurrence");
            }
            builder
                .try_insert_last(key("key-00"), Value::Bool(true))
                .expect("fixture inserts final duplicate");
            let object = builder.try_finish().expect("fixture finishes");

            assert_eq!(object.len(), unique);
            assert_eq!(object.get_index(0).expect("first entry exists").key(), "key-00");
            assert!(matches!(object.get("key-00"), Some(Value::Bool(true))));
            assert_eq!(
                matches!(&object.0.index, Some(ObjectStorageIndex::Lookup(_))),
                unique > LINEAR_DEDUP_THRESHOLD
            );
        }
    }

    /// Equal allocation keys alias one entry table, and independently built tables never share a key — the O(1)
    /// witness behind `shares_storage_with`.
    #[test]
    fn allocation_keys_agree_exactly_with_storage_sharing() {
        let original = finish_unique(4);
        let twin = original.clone();
        assert!(original.shares_storage_with(&twin));
        assert_eq!(original.allocation_key(), twin.allocation_key());

        let other = finish_unique(4);
        assert!(!original.shares_storage_with(&other));
        assert_ne!(original.allocation_key(), other.allocation_key());
    }

    /// A copy-on-write detach RETAINS the source's key allocations rather than copying their text: every key in the
    /// detached table is the same allocation, in the same insertion position, holding the same text. The detach still
    /// forks the entry table, which is what makes the two objects independent — the shared key is a payload, not a
    /// link between them.
    #[test]
    fn a_detached_entry_table_retains_the_source_key_allocations() {
        let original = finish_unique(LINEAR_DEDUP_THRESHOLD + 1);
        let mut detached = original.clone();
        *detached
            .try_get_index_mut(0)
            .expect("COW detach succeeds")
            .expect("entry exists") = Value::Null;

        assert!(!detached.shares_storage_with(&original));
        assert_eq!(detached.len(), original.len());
        for position in 0..original.len() {
            let source = original.get_index(position).expect("source entry exists");
            let forked = detached.get_index(position).expect("forked entry exists");
            assert_eq!(source.key(), forked.key());
            let source_key = source.clone_key();
            let forked_key = forked.clone_key();
            // A detach either retains the source's key allocation (boxed arm) or copies an inline key's text, which
            // costs no allocation at all — both preserve the text without a fresh heap copy.
            assert!(
                source_key.shares_text_with(&forked_key) || (source_key.is_inline() && forked_key.is_inline()),
                "the detached table copied the key text at position {position}"
            );
        }

        // An independently built key with the same text is a DIFFERENT allocation, so the witness above is testing
        // sharing rather than agreeing with itself.
        let twin = key(original.get_index(0).expect("first entry exists").key());
        assert!(
            !original
                .get_index(0)
                .expect("first entry exists")
                .clone_key()
                .shares_text_with(&twin)
        );
    }
}
