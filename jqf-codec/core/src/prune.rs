//! The kept-subtree prune HINT a whole-document requirement may carry.
//!
//! The tree names which document subtrees the requesting program can read: an `all` node keeps its whole subtree, named
//! keys and the shared element node keep their children selectively, and an object member matching NEITHER a named key
//! nor the element node is unobservable by the program and MAY be omitted from the decoded value. Arrays never omit
//! elements (omission would shift indices and counts); pruning inside an array happens per element through the element
//! node. Scalars at kept positions are always delivered verbatim.
//!
//! The hint is MONOTONE: delivering more than the tree names is always sound, so a codec is free to ignore it entirely.
//! The consumers that do consult it each keep their own flattened copy of this tree — the retained-source document
//! decoders, and the builtins' decode path over pulled records. Nothing pre-folds the element demand into the named
//! keys: the join happens AT lookup, here in [`PruneTreeNode::member`] (exact hit, else the element node) and again in
//! every consumer-side copy's own table.

use alloc::string::String;
use alloc::vec::Vec;
use jqf_resource::{ResourceContext, ResourceError};

use crate::pattern::{ExactPath, PortableStep};

/// A hard ceiling on transported tree size; the engine's own walk budget sits well below it, so hitting this is a
/// producer contract violation.
pub(crate) const MAX_PRUNE_TREE_NODES: usize = 4096;

/// Failure to construct a canonical prune tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PruneTreeError {
    /// The tree already holds [`MAX_PRUNE_TREE_NODES`] nodes.
    TooManyNodes,
    /// A key was pushed out of byte order, which would break lookup.
    UnorderedKey,
    /// A child or node id does not name an existing node.
    UnknownNode,
    /// The request ledger rejected the tree's retained storage.
    Resource(ResourceError),
}

impl From<ResourceError> for PruneTreeError {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

/// One node of the kept-subtree selection: the value at this position is kept whole (`all`), or selectively through
/// named keys and the element node.
#[derive(Debug)]
pub struct PruneTreeNode {
    all: bool,
    element: Option<u32>,
    /// Named member demands, sorted by name bytes (the push order contract).
    keys: Vec<PruneTreeKey>,
}

#[derive(Debug)]
struct PruneTreeKey {
    name: String,
    child: u32,
}

impl PruneTreeNode {
    /// Whether the whole subtree at this position must be delivered.
    #[must_use]
    pub const fn is_all(&self) -> bool {
        self.all
    }

    /// The shared every-child demand node, when one exists.
    #[must_use]
    pub const fn element(&self) -> Option<u32> {
        self.element
    }

    /// The named member's demand node: an exact hit, else the element node, else `None` — and `None` means the member
    /// is unobservable and may be omitted.
    #[must_use]
    pub fn member(&self, name: &[u8]) -> Option<u32> {
        self.keys
            .as_slice()
            .binary_search_by(|key| key.name.as_bytes().cmp(name))
            .ok()
            .map(|position| self.keys.as_slice()[position].child)
            .or(self.element)
    }

    /// The named member demands in ascending key order — the consumer-side copy walks this to build its own lookup
    /// table.
    pub fn members(&self) -> impl Iterator<Item = (&str, u32)> {
        self.keys.as_slice().iter().map(|key| (key.name.as_str(), key.child))
    }
}

/// The transported kept-subtree selection: a flat arena of nodes with the document root at index 0.
#[derive(Debug)]
pub struct PruneTree {
    nodes: Vec<PruneTreeNode>,
}

impl PruneTree {
    /// The document root's node id.
    pub const ROOT: u32 = 0;

    /// Creates a tree holding only a keep-nothing root; the producer adds children and marks `all` positions before
    /// attaching it.
    pub fn try_new(_resources: &ResourceContext<'_>) -> Result<Self, PruneTreeError> {
        let mut tree = Self { nodes: Vec::new() };
        tree.try_push_node(false)?;
        Ok(tree)
    }

    /// A prune tree that keeps only the spine of `path`: each member step is one named key, each array index is the
    /// shared element child (arrays never omit elements). The selected leaf is kept whole.
    ///
    /// Empty and range paths return `None` — the empty path is the whole document, and a range step has no prune-tree
    /// spelling.
    pub(crate) fn try_from_exact_path(
        path: &ExactPath,
        resources: &ResourceContext<'_>,
    ) -> Result<Option<Self>, PruneTreeError> {
        if path.is_root() || path.has_semantic_range() {
            return Ok(None);
        }
        let mut tree = Self::try_new(resources)?;
        let mut current = Self::ROOT;
        let steps = path.steps();
        for (index, step) in steps.iter().enumerate() {
            let child = tree.try_push_node(index + 1 == steps.len())?;
            match step {
                PortableStep::SemanticMember(key) => {
                    tree.try_push_key(current, key.as_str(), child)?;
                }
                PortableStep::SemanticIndex(_) => {
                    tree.try_set_element(current, child)?;
                }
                PortableStep::SemanticRange { .. } => return Ok(None),
            }
            current = child;
        }
        Ok(Some(tree))
    }

    /// Identity of this tree for session recycle: two trees that name different kept subtrees must not share a residual
    /// key.
    pub(crate) fn fingerprint(&self) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325;
        mix_u32(&mut hash, u32::try_from(self.nodes.len()).unwrap_or(u32::MAX));
        for node in self.nodes.as_slice() {
            mix(&mut hash, u8::from(node.all));
            match node.element {
                Some(id) => {
                    mix(&mut hash, 1);
                    mix_u32(&mut hash, id);
                }
                None => mix(&mut hash, 0),
            }
            mix_u32(&mut hash, u32::try_from(node.keys.len()).unwrap_or(u32::MAX));
            for key in node.keys.as_slice() {
                mix_u32(&mut hash, u32::try_from(key.name.len()).unwrap_or(u32::MAX));
                mix_bytes(&mut hash, key.name.as_bytes());
                mix_u32(&mut hash, key.child);
            }
        }
        hash
    }

    /// Recycle identity of a path-derived spine, without building the tree. `None` for the empty path and for any range
    /// step.
    pub(crate) fn spine_fingerprint(path: &ExactPath) -> Option<u64> {
        if path.is_root() || path.has_semantic_range() {
            return None;
        }
        let mut hash = 0x9e37_79b9_7f4a_7c15;
        for step in path.steps() {
            match step {
                PortableStep::SemanticMember(key) => {
                    mix(&mut hash, 0);
                    mix_u32(&mut hash, u32::try_from(key.len()).unwrap_or(u32::MAX));
                    mix_bytes(&mut hash, key.as_bytes());
                }
                PortableStep::SemanticIndex(_) => mix(&mut hash, 1),
                PortableStep::SemanticRange { .. } => return None,
            }
        }
        Some(hash)
    }

    /// Appends one node and returns its id.
    ///
    /// Producer invariant: a child is pushed BEFORE its parent's edges name it, so every element/key edge points to a
    /// strictly higher id and the graph is acyclic by construction. The checks below reject an out-of-range id, never a
    /// backward or cyclic one; consumers walk ids upward and trust this order. Append-only pushes also keep ids dense
    /// from zero — the contiguity consumers' transport scans infer.
    pub fn try_push_node(&mut self, all: bool) -> Result<u32, PruneTreeError> {
        if self.nodes.len() >= MAX_PRUNE_TREE_NODES {
            return Err(PruneTreeError::TooManyNodes);
        }
        // MAX_PRUNE_TREE_NODES bounds len() far below u32::MAX, so this is a formality the clippy floor requires over a
        // truncating cast.
        let id = u32::try_from(self.nodes.len()).map_err(|_| PruneTreeError::TooManyNodes)?;
        self.nodes.push(PruneTreeNode {
            all,
            element: None,
            keys: Vec::new(),
        });
        Ok(id)
    }

    /// Names `node`'s shared every-child demand.
    pub fn try_set_element(&mut self, node: u32, child: u32) -> Result<(), PruneTreeError> {
        if child as usize >= self.nodes.len() {
            return Err(PruneTreeError::UnknownNode);
        }
        let entry = self
            .nodes
            .as_mut_slice()
            .get_mut(node as usize)
            .ok_or(PruneTreeError::UnknownNode)?;
        entry.element = Some(child);
        Ok(())
    }

    /// Appends one named member demand to `node`. Keys MUST arrive in strictly ascending byte order — the lookup is a
    /// binary search.
    pub fn try_push_key(&mut self, node: u32, name: &str, child: u32) -> Result<(), PruneTreeError> {
        // One bound check serves both steps: a valid `child` proves the node id is in range too, since the child was
        // pushed after its parent.
        if child as usize >= self.nodes.len() {
            return Err(PruneTreeError::UnknownNode);
        }
        let entry = self
            .nodes
            .as_mut_slice()
            .get_mut(node as usize)
            .ok_or(PruneTreeError::UnknownNode)?;
        if let Some(last) = entry.keys.as_slice().last()
            && last.name.as_bytes() >= name.as_bytes()
        {
            return Err(PruneTreeError::UnorderedKey);
        }
        let mut stored = String::new();
        stored
            .try_reserve_exact(name.len())
            .map_err(jqf_resource::ResourceError::from)?;
        stored.push_str(name);
        // The keys Vec grows on the fallible path exactly as the key String does: an untracked push here would
        // contradict the tree's own reserve law (the counting allocator refuses the reservation under failure
        // injection).
        entry.keys.try_reserve(1).map_err(jqf_resource::ResourceError::from)?;
        entry.keys.push(PruneTreeKey { name: stored, child });
        Ok(())
    }

    /// Clones this tree into `resources`' ledger — the residual-requirement reconstruction must carry the hint
    /// explicitly, exactly as it carries the lazy-frontier depth.
    pub fn try_clone_in(&self, resources: &ResourceContext<'_>) -> Result<Self, PruneTreeError> {
        let mut clone = Self::try_new(resources)?;
        for (id, node) in self.nodes.as_slice().iter().enumerate() {
            if id == 0 {
                clone.nodes.as_mut_slice()[0].all = node.all;
            } else {
                clone.try_push_node(node.all)?;
            }
        }
        for (id, node) in self.nodes.as_slice().iter().enumerate() {
            let id = u32::try_from(id).map_err(|_| PruneTreeError::TooManyNodes)?;
            if let Some(element) = node.element {
                clone.try_set_element(id, element)?;
            }
            for key in node.keys.as_slice() {
                clone.try_push_key(id, key.name.as_str(), key.child)?;
            }
        }
        Ok(clone)
    }

    /// The node at `id`; `None` for an id this tree never minted.
    #[must_use]
    pub fn node(&self, id: u32) -> Option<&PruneTreeNode> {
        self.nodes.as_slice().get(id as usize)
    }

    /// The document root's node.
    #[must_use]
    pub fn root(&self) -> &PruneTreeNode {
        &self.nodes.as_slice()[Self::ROOT as usize]
    }
}

fn mix(hash: &mut u64, byte: u8) {
    *hash ^= u64::from(byte);
    *hash = hash.wrapping_mul(0x100_0000_01b3);
}

fn mix_u32(hash: &mut u64, value: u32) {
    mix_bytes(hash, &value.to_le_bytes());
}

fn mix_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        mix(hash, *byte);
    }
}

#[cfg(test)]
mod tests {
    use super::{PruneTree, PruneTreeError};
    use crate::test_support::resources;

    #[test]
    fn member_lookup_is_hit_then_element_then_omit() {
        let resources = resources();
        let mut tree = PruneTree::try_new(&resources).expect("tree");
        let id_node = tree.try_push_node(true).expect("node");
        let element = tree.try_push_node(false).expect("node");
        tree.try_push_key(PruneTree::ROOT, "id", id_node).expect("key");
        tree.try_set_element(element, id_node).expect("element");
        let root = tree.root();
        assert_eq!(root.member(b"id"), Some(id_node));
        assert_eq!(root.member(b"name"), None);
        let element = tree.node(element).expect("element node");
        assert_eq!(element.member(b"anything"), Some(id_node));
    }

    #[test]
    fn exact_path_spine_keeps_member_keys_and_index_as_element() {
        let resources = resources();
        let mut path = crate::pattern::ExactPath::try_new(&resources);
        path.try_push_semantic_member("users", &resources).expect("users");
        path.try_push_semantic_index(0, &resources);
        path.try_push_semantic_member("id", &resources).expect("id");
        let tree = PruneTree::try_from_exact_path(&path, &resources)
            .expect("spine")
            .expect("nonempty");
        let users = tree.root().member(b"users").expect("users key");
        assert!(tree.root().member(b"orders").is_none());
        let users_node = tree.node(users).expect("users node");
        let element = users_node.element().expect("index is element");
        let element_node = tree.node(element).expect("element node");
        let id = element_node.member(b"id").expect("id key");
        assert!(tree.node(id).expect("id").is_all());
        assert!(
            PruneTree::try_from_exact_path(&crate::pattern::ExactPath::try_new(&resources), &resources)
                .expect("empty")
                .is_none()
        );
    }

    #[test]
    fn unordered_keys_are_rejected() {
        let resources = resources();
        let mut tree = PruneTree::try_new(&resources).expect("tree");
        let child = tree.try_push_node(true).expect("node");
        tree.try_push_key(PruneTree::ROOT, "b", child).expect("key");
        assert_eq!(
            tree.try_push_key(PruneTree::ROOT, "a", child),
            Err(PruneTreeError::UnorderedKey)
        );
        assert_eq!(
            tree.try_push_key(PruneTree::ROOT, "b", child),
            Err(PruneTreeError::UnorderedKey)
        );
    }
}
