//! Object lookup: a compact table at finish, or a tree after a write.
//!
//! Wide objects finish with an Eytzinger table. A later insert or a copy-on-write detach switches to an incremental
//! tree. Both compare the first eight bytes of a key before reading the rest.

use alloc::vec::Vec;
use core::cmp::Ordering;

use super::ObjectEntry;

/// The live index shape: a finished Eytzinger table or an incremental tree, selected by mutation state.
#[derive(Clone, Debug)]
pub(super) enum ObjectStorageIndex {
    Lookup(ObjectLookupIndex),
    Incremental(ObjectIncrementalIndex),
}

/// The finished wide-object index: entry positions in Eytzinger order.
#[derive(Clone, Debug)]
pub(super) struct ObjectLookupIndex {
    /// Entry positions in Eytzinger order for cache-friendly binary lookup.
    pub(super) positions: Vec<usize>,
}

/// The mutation-safe index: a balanced tree grown in place after a copy-on-write detach or insertion.
#[derive(Clone, Debug, Default)]
pub(super) struct ObjectIncrementalIndex {
    pub(super) nodes: Vec<ObjectIndexNode>,
    pub(super) root: Option<usize>,
}

/// One incremental-tree node: position, prefix word, children, and subtree height (the AVL balance).
#[derive(Clone, Debug)]
pub(super) struct ObjectIndexNode {
    position: usize,
    /// The node key's [`key_prefix`], compared BEFORE the key text: most walks resolve their direction on this one
    /// integer and never touch the entry's string. Equal prefixes (keys sharing their first eight bytes, or a shorter
    /// key against its own zero padding) fall back to the full comparison, so the tree's order is exactly the string
    /// order it always was — the prefix is a fast first word of that order, not a second one.
    prefix: u64,
    left: Option<usize>,
    right: Option<usize>,
    height: usize,
}

/// The first eight bytes of `key`, zero-padded, as a big-endian word.
///
/// Big-endian keeps the integer order lexicographic: whenever two keys' prefixes differ, `prefix(a) < prefix(b)` agrees
/// with `a < b` byte for byte, and the zero padding sorts a short key before every longer key it prefixes (`"ab" <
/// "abc"` because `0x00 < b'c'`). Only equal prefixes say nothing — including the one ambiguity padding introduces, a
/// key whose ninth byte onward differs or a literal NUL against padding — and every such tie is resolved by the
/// caller's full string comparison.
fn key_prefix(key: &str) -> u64 {
    let bytes = key.as_bytes();
    let mut word = [0_u8; 8];
    let len = bytes.len().min(8);
    word[..len].copy_from_slice(&bytes[..len]);
    u64::from_be_bytes(word)
}

#[derive(Clone, Copy)]
enum Direction {
    Left,
    Right,
}

/// What the shared AVL walk's Equal arm does with an existing key.
#[derive(Clone, Copy)]
enum OnExisting {
    /// Report the existing position; the tree is unchanged.
    Report,
    /// Move the existing key's position (the caller re-ordered entries).
    Reposition,
}

/// Builds the incremental tree from already-unique entries (the small-object path).
pub(super) fn build_unique_incremental_index(
    entries: &[ObjectEntry],
) -> Result<ObjectIncrementalIndex, alloc::collections::TryReserveError> {
    let mut index = ObjectIncrementalIndex::default();
    for (position, entry) in entries.iter().enumerate() {
        index.try_insert(entries, entry.key.as_str(), position)?;
    }
    Ok(index)
}

/// Converts a finished Eytzinger table into an incremental tree, reserving room for `additional` insertions.
pub(super) fn build_incremental_from_lookup(
    lookup: &ObjectLookupIndex,
    entries: &[ObjectEntry],
    additional: usize,
) -> Result<ObjectIncrementalIndex, alloc::collections::TryReserveError> {
    let mut nodes = Vec::new();
    nodes.try_reserve_exact(lookup.positions.len().saturating_add(additional))?;
    for (slot, &position) in lookup.positions.iter().enumerate() {
        let left = slot.checked_mul(2).and_then(|slot| slot.checked_add(1));
        let right = slot.checked_mul(2).and_then(|slot| slot.checked_add(2));
        nodes.push(ObjectIndexNode {
            position,
            prefix: key_prefix(entries[position].key.as_str()),
            left: left.filter(|&child| child < lookup.positions.len()),
            right: right.filter(|&child| child < lookup.positions.len()),
            height: 1,
        });
    }
    for node in (0..nodes.len()).rev() {
        let left = nodes[node].left.map_or(0, |child| nodes[child].height);
        let right = nodes[node].right.map_or(0, |child| nodes[child].height);
        nodes[node].height = 1 + left.max(right);
    }
    Ok(ObjectIncrementalIndex {
        root: (!nodes.is_empty()).then_some(0),
        nodes,
    })
}

impl ObjectStorageIndex {
    pub(super) fn get(&self, entries: &[ObjectEntry], key: &str) -> Option<usize> {
        match self {
            Self::Lookup(index) => index.get(entries, key),
            Self::Incremental(index) => index.get(entries, key),
        }
    }

    /// One walk: `Ok(Some(pos))` if `key` is already present, `Ok(None)` if it was inserted at `position`.
    pub(super) fn try_occupy_or_insert(
        &mut self,
        entries: &[ObjectEntry],
        key: &str,
        position: usize,
    ) -> Result<Option<usize>, alloc::collections::TryReserveError> {
        match self {
            Self::Lookup(lookup) => {
                if let Some(existing) = lookup.get(entries, key) {
                    return Ok(Some(existing));
                }
                let mut index = build_incremental_from_lookup(lookup, entries, 1)?;
                index.try_insert(entries, key, position)?;
                *self = Self::Incremental(index);
                Ok(None)
            }
            Self::Incremental(index) => index.try_occupy_or_insert(entries, key, position),
        }
    }
}

impl ObjectLookupIndex {
    /// Builds the Eytzinger table over unique entries, reserving room for `additional` positions.
    pub(super) fn try_from_unique_entries(
        entries: &[ObjectEntry],
        additional: usize,
    ) -> Result<Self, alloc::collections::TryReserveError> {
        let mut sorted = Vec::new();
        sorted.try_reserve_exact(entries.len().saturating_add(additional))?;
        sorted.extend(0..entries.len());
        sorted.sort_unstable_by(|left, right| entries[*left].key.as_str().cmp(entries[*right].key.as_str()));

        let mut positions = Vec::new();
        positions.try_reserve_exact(sorted.len().saturating_add(additional))?;
        positions.resize(sorted.len(), 0);
        crate::index::try_fill_eytzinger_by(&mut positions, |index| sorted[index])
            .expect("usize positions represent every usize index");
        Ok(Self { positions })
    }

    /// A position this index holds but the entry table does not have answers "absent" rather than panicking — the
    /// same fail-closed direction the located twin takes, for a mismatch that construction already denies.
    fn get(&self, entries: &[ObjectEntry], key: &str) -> Option<usize> {
        let prefix = key_prefix(key);
        crate::index::find_eytzinger(&self.positions, |position| {
            entries
                .get(position)
                .map(|entry| {
                    let stored = entry.key.as_str();
                    // Eytzinger walks `stored.cmp(search)`; the prefix word is the same orientation so a short-circuit
                    // cannot invert a direction the tree was sorted in.
                    key_prefix(stored).cmp(&prefix).then_with(|| stored.cmp(key))
                })
                .ok_or(())
        })
        .unwrap_or(None)
    }
}

impl ObjectIncrementalIndex {
    pub(super) fn get(&self, entries: &[ObjectEntry], key: &str) -> Option<usize> {
        let prefix = key_prefix(key);
        let mut cursor = self.root;
        while let Some(node) = cursor {
            let node = &self.nodes[node];
            // The prefix word decides most directions without reading the entry's text; only an equal prefix consults
            // the full key.
            let ordering = prefix
                .cmp(&node.prefix)
                .then_with(|| key.cmp(entries[node.position].key.as_str()));
            match ordering {
                Ordering::Less => cursor = node.left,
                Ordering::Equal => return Some(node.position),
                Ordering::Greater => cursor = node.right,
            }
        }
        None
    }

    /// The one AVL walk behind [`Self::try_occupy_or_insert`] and [`Self::try_insert`]: descend by prefix-then-key, run
    /// the Equal-arm policy on a hit, else insert `position` and rebalance up the path.
    ///
    /// The AVL invariant bounds the height at ~1.44·log2(n), so no reachable tree fills the path bound; it exists so a
    /// balance bug fails loudly here rather than walking past the array.
    fn walk_insert(
        &mut self,
        entries: &[ObjectEntry],
        key: &str,
        position: usize,
        on_existing: OnExisting,
    ) -> Result<Option<usize>, alloc::collections::TryReserveError> {
        const MAX_PATH: usize = 128;
        let prefix = key_prefix(key);
        let mut path = [None; MAX_PATH];
        let mut path_len = 0;
        let mut cursor = self.root;
        while let Some(node) = cursor {
            let ordering = prefix
                .cmp(&self.nodes[node].prefix)
                .then_with(|| key.cmp(entries[self.nodes[node].position].key.as_str()));
            match ordering {
                Ordering::Less => {
                    path[path_len] = Some((node, Direction::Left));
                    path_len += 1;
                    cursor = self.nodes[node].left;
                }
                Ordering::Equal => match on_existing {
                    OnExisting::Report => return Ok(Some(self.nodes[node].position)),
                    OnExisting::Reposition => {
                        self.nodes[node].position = position;
                        return Ok(None);
                    }
                },
                Ordering::Greater => {
                    path[path_len] = Some((node, Direction::Right));
                    path_len += 1;
                    cursor = self.nodes[node].right;
                }
            }
        }

        self.nodes.try_reserve(1)?;
        let node = self.nodes.len();
        self.nodes.push(ObjectIndexNode {
            position,
            prefix,
            left: None,
            right: None,
            height: 1,
        });

        let mut subtree = node;
        while path_len > 0 {
            path_len -= 1;
            let (parent, direction) = path[path_len].expect("the traversed path slot is populated");
            match direction {
                Direction::Left => self.nodes[parent].left = Some(subtree),
                Direction::Right => self.nodes[parent].right = Some(subtree),
            }
            subtree = self.rebalance(parent);
        }
        self.root = Some(subtree);
        Ok(None)
    }

    /// One walk: `Ok(Some(pos))` if `key` is already present, `Ok(None)` after inserting `position`.
    pub(super) fn try_occupy_or_insert(
        &mut self,
        entries: &[ObjectEntry],
        key: &str,
        position: usize,
    ) -> Result<Option<usize>, alloc::collections::TryReserveError> {
        self.walk_insert(entries, key, position, OnExisting::Report)
    }

    /// Inserts or moves a position in the tree, rebalancing up the traversed path (AVL).
    pub(super) fn try_insert(
        &mut self,
        entries: &[ObjectEntry],
        key: &str,
        position: usize,
    ) -> Result<(), alloc::collections::TryReserveError> {
        self.walk_insert(entries, key, position, OnExisting::Reposition)
            .map(|_| ())
    }

    fn rebalance(&mut self, node: usize) -> usize {
        self.update_height(node);
        let (left_height, right_height) = self.heights(node);
        if left_height > right_height + 1 {
            let left = self.nodes[node].left.expect("taller left subtree has left child");
            let (inner_left, inner_right) = self.heights(left);
            if inner_left < inner_right {
                self.nodes[node].left = Some(self.rotate_left(left));
            }
            return self.rotate_right(node);
        }
        if right_height > left_height + 1 {
            let right = self.nodes[node].right.expect("taller right subtree has right child");
            let (inner_left, inner_right) = self.heights(right);
            if inner_left > inner_right {
                self.nodes[node].right = Some(self.rotate_right(right));
            }
            return self.rotate_left(node);
        }
        node
    }

    fn rotate_left(&mut self, node: usize) -> usize {
        let root = self.nodes[node].right.expect("left rotation requires right child");
        let middle = self.nodes[root].left;
        self.nodes[node].right = middle;
        self.update_height(node);
        self.nodes[root].left = Some(node);
        self.update_height(root);
        root
    }

    fn rotate_right(&mut self, node: usize) -> usize {
        let root = self.nodes[node].left.expect("right rotation requires left child");
        let middle = self.nodes[root].right;
        self.nodes[node].left = middle;
        self.update_height(node);
        self.nodes[root].right = Some(node);
        self.update_height(root);
        root
    }

    fn update_height(&mut self, node: usize) {
        self.nodes[node].height = 1 + self
            .height(self.nodes[node].left)
            .max(self.height(self.nodes[node].right));
    }

    fn heights(&self, node: usize) -> (usize, usize) {
        (self.height(self.nodes[node].left), self.height(self.nodes[node].right))
    }

    fn height(&self, node: Option<usize>) -> usize {
        node.map_or(0, |node| self.nodes[node].height)
    }
}

#[cfg(test)]
mod tests {
    use super::{ObjectEntry, ObjectIncrementalIndex};
    use crate::{ObjectKey, Value};
    use alloc::vec::Vec;

    fn entries(count: usize) -> Vec<ObjectEntry> {
        (0..count)
            .map(|position| ObjectEntry {
                key: ObjectKey::try_from_str(&alloc::format!("key-{position:05}")).expect("key"),
                value: Value::Bool(false),
            })
            .collect()
    }

    /// A fixed Fisher-Yates shuffle over a simple LCG, deterministic for the test and independent of any generator the
    /// crate might grow.
    fn shuffled(count: usize) -> Vec<usize> {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut order: Vec<usize> = (0..count).collect();
        for index in (1..count).rev() {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let swap = ((state >> 33) as usize) % (index + 1);
            order.swap(index, swap);
        }
        order
    }

    /// Walks the whole tree asserting the AVL balance law at every node and the reachability of every stored node.
    fn assert_avl(index: &ObjectIncrementalIndex) {
        let mut seen = 0;
        let mut stack = Vec::new();
        if let Some(root) = index.root {
            stack.push(root);
        }
        while let Some(slot) = stack.pop() {
            seen += 1;
            let node = &index.nodes[slot];
            let left = node.left.map_or(0, |child| index.nodes[child].height);
            let right = node.right.map_or(0, |child| index.nodes[child].height);
            assert!(
                left.abs_diff(right) <= 1,
                "balance violated at node {slot}: left {left} right {right}"
            );
            if let Some(child) = node.left {
                stack.push(child);
            }
            if let Some(child) = node.right {
                stack.push(child);
            }
        }
        assert_eq!(seen, index.nodes.len(), "every node reachable from the root");
        let height = index.root.map_or(0, |root| index.nodes[root].height);
        // The AVL bound for 128 keys is ~10; 12 catches a degenerating tree while leaving slack for the exact constant.
        assert!(
            height <= 12,
            "height {height} for {} keys is not AVL",
            index.nodes.len()
        );
    }

    /// The AVL invariant is what the incremental index's membership law rests on, and no other test observes it: a
    /// rotation bug that keeps the tree ordered but unbalanced would pass every lookup test. Inserting keys in the two
    /// degenerate orders and a fixed shuffle must keep every node balanced and the height logarithmic.
    #[test]
    fn incremental_index_stays_balanced_under_adversarial_insert_orders() {
        const COUNT: usize = 128;
        let entries = entries(COUNT);

        let mut ascending = ObjectIncrementalIndex::default();
        for position in 0..COUNT {
            ascending
                .try_insert(&entries, entries[position].key.as_str(), position)
                .expect("insert");
        }
        assert_avl(&ascending);

        let mut descending = ObjectIncrementalIndex::default();
        for position in (0..COUNT).rev() {
            descending
                .try_insert(&entries, entries[position].key.as_str(), position)
                .expect("insert");
        }
        assert_avl(&descending);

        let mut shuffled_index = ObjectIncrementalIndex::default();
        for position in shuffled(COUNT) {
            shuffled_index
                .try_insert(&entries, entries[position].key.as_str(), position)
                .expect("insert");
        }
        assert_avl(&shuffled_index);
    }
}
