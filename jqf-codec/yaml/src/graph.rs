//! The YAML document graph: one compact arena per parsed document.
//!
//! The storage is packed for the 100k-record catalog profile: a `NodeRecord` is a 32-byte enum (kind + a text
//! reference + a flat-arena range + packed property ids + a source span), against the ~128-byte variant with three
//! `Option<String>` fields and a per-mapping `Vec` the previous design carried. Scalar text lives as [`TextRef`]: a
//! source `Span` when the decoded text IS the source bytes (the zero-copy route), else an index into the owned-text
//! arena. Sequence items and mapping entries are FLAT arenas; each container node names its `[start, len)` range into
//! them, so a mapping with five entries pays one node record plus five 8-byte pairs instead of a per-mapping
//! allocation.
//!
//! Consumers read through [`YamlGraph::node`], which materializes a BORROWED [`YamlNode`] view (text resolved from the
//! source or the text arena). The parser constructs through the `add_*` methods, which append to the flat arenas and
//! intern property names in the name arena; name lookup is map-backed ([`YamlGraph::intern_name`] consults an
//! exact-text sidecar), so interning costs O(log n) per call instead of a scan over every distinct name already in the
//! arena.

use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_source::{ResolvedSource, Span};

/// One node's identity in the graph arena.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct NodeId(pub(crate) u32);

impl NodeId {
    #[must_use]
    pub(crate) fn index(self) -> usize {
        usize::try_from(self.0).expect("node id fits usize")
    }
}

/// Where a decoded scalar's text lives.
#[derive(Clone, Copy, Debug)]
pub(crate) enum TextRef {
    /// The decoded text IS these exact source bytes: the document names the span instead of copying the text (the
    /// source-span zero-copy route). The span is in ORIGINAL source bytes.
    Span(Span),
    /// The text is owned in the graph's text arena (escaped/folded content whose bytes differ from the source).
    Owned(u32),
}

/// The node kinds, stored one byte per node.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NodeKind {
    Scalar,
    Sequence,
    Mapping,
    Alias,
}

/// One compact node record.
///
/// The fields are kind-specific: scalars carry `text` + `style`, sequences and mappings carry `range` (into the flat
/// `children`/`entries` arenas), and aliases carry `target` plus the source span of the alias token (the edit lane's
/// container-span pass needs the alias SITE's position — the shared target's own span is the anchor's, elsewhere in the
/// document). `props` packs the interned tag and anchor ids (0 = none, ids offset by one).
#[derive(Clone, Copy, Debug)]
pub(crate) enum NodeRecord {
    Scalar {
        text: TextRef,
        style: u8,
        props: u32,
        span: Span,
    },
    Sequence {
        range: (u32, u32),
        props: u32,
        span: Span,
    },
    Mapping {
        range: (u32, u32),
        props: u32,
        span: Span,
    },
    Alias(NodeId, Span),
}

/// The scalar styles (mirrors `crate::scan::ScalarStyle` without the import cycle; the values are the same).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum ScalarStyle {
    Plain = 0,
    SingleQuoted = 1,
    DoubleQuoted = 2,
    Literal = 3,
    Folded = 4,
}

impl ScalarStyle {
    #[must_use]
    pub(crate) const fn from_u8(value: u8) -> Self {
        match value {
            0 | 5.. => Self::Plain,
            1 => Self::SingleQuoted,
            2 => Self::DoubleQuoted,
            3 => Self::Literal,
            4 => Self::Folded,
        }
    }
}

/// A borrowed view of one node, resolved from the graph arena plus the source. The field names mirror the previous
/// owned `YamlNode` so consumer match arms keep their shape; the collection payloads are slices into the flat arenas
/// and the text is resolved to `&str`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum YamlNode<'a> {
    /// A scalar with its style and exact resolved tag (when explicit).
    Scalar {
        text: &'a str,
        style: ScalarStyle,
        /// The resolved exact tag text (`tag:yaml.org,2002:str`, `!money`, `!<...>` verbatim), when the node carried an
        /// explicit tag.
        tag: Option<&'a str>,
        /// The node's anchor name, when it carries `&name`.
        anchor: Option<&'a str>,
        /// The original source span of the scalar's content.
        span: Span,
    },
    /// A sequence with its ordered items and tag/anchor properties.
    Sequence {
        items: &'a [NodeId],
        tag: Option<&'a str>,
        anchor: Option<&'a str>,
        span: Span,
    },
    /// A mapping with its ordered key/value pairs and tag/anchor properties.
    Mapping {
        entries: &'a [(NodeId, NodeId)],
        tag: Option<&'a str>,
        anchor: Option<&'a str>,
        span: Span,
    },
    /// An alias occurrence referencing a shared node. Aliases never carry properties of their own; the alias TOKEN span
    /// (`*name`) lives on the graph record and is read through [`YamlGraph::node_span`].
    Alias(NodeId),
}

/// One source-ordered anchor binding inside one document.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AnchorBinding {
    /// The anchor name (interned in the graph's name arena).
    pub(crate) name: u32,
    /// The node the anchor names.
    pub(crate) node: NodeId,
}

/// The packed tag/anchor property ids in a node record (0 = none, ids are offset by one so id 0 cannot collide with the
/// interner's first entry).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NodeProps {
    tag: u16,
    anchor: u16,
}

/// The largest name-arena id [`NodeProps::encode`] can carry exactly: the packed node record holds each id in 16 bits
/// with 0 reserved as "none", so an id past this bound would silently alias every later tag/anchor onto another name.
/// The interner refuses the document instead.
pub(crate) const MAX_NAME_ID: u32 = 0xfffd;

impl NodeProps {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the guard `id < 0xffff` bounds the cast; [`intern_name`] refuses ids past \
                 `MAX_NAME_ID`, so the fallback arm is unreachable from a built graph"
    )]
    const fn encode(tag: Option<u32>, anchor: Option<u32>) -> u32 {
        let tag = match tag {
            Some(id) if id < 0xffff => (id + 1) as u16,
            Some(_) => u16::MAX,
            None => 0,
        };
        let anchor = match anchor {
            Some(id) if id < 0xffff => (id + 1) as u16,
            Some(_) => u16::MAX,
            None => 0,
        };
        ((anchor as u32) << 16) | (tag as u32)
    }

    fn tag_id(self) -> Option<u32> {
        let raw = u32::from(self.tag);
        if raw != 0 { Some(raw - 1) } else { None }
    }

    fn anchor_id(self) -> Option<u32> {
        let raw = u32::from(self.anchor);
        if raw != 0 { Some(raw - 1) } else { None }
    }
}

/// The YAML graph arena for one document.
pub(crate) struct YamlGraph {
    nodes: Vec<NodeRecord>,
    /// The document ROOT node: the node whose collection closed with no open parent (the parser records it; the arena
    /// alone cannot name it, since the last node added is a scalar deep inside the document).
    root: Option<NodeId>,
    /// Source-ordered anchor binding history for THIS document (reset at each document boundary by the caller
    /// constructing a fresh graph).
    anchors: Vec<AnchorBinding>,
    /// The name arena (exact strings, interned per graph; anchor AND tag names share it).
    names: Vec<String>,
    /// Lookup sidecar over `names`: exact text -> id. `intern_name` consults this instead of scanning the arena, which
    /// is quadratic in the number of DISTINCT names; a document with many distinct anchors or local tags pays O(log n)
    /// per intern instead of O(names).
    name_ids: BTreeMap<Box<str>, u32>,
    /// Owned scalar texts (escaped/folded content; the zero-copy spans do not occupy it).
    texts: Vec<alloc::boxed::Box<str>>,
    /// Flat sequence-item arena; each sequence node names a range into it.
    children: Vec<NodeId>,
    /// Flat mapping-entry arena; each mapping node names a range into it.
    entries: Vec<(NodeId, NodeId)>,
    /// One comment record per `#` comment in the document, in source order. The scanner records the span as trivia is
    /// skipped; the whole-document walker attaches each span to the node that follows it (the cross-format
    /// leading-comment model) as a `yaml.comment@1` fact.
    comments: Vec<Span>,
    /// Merge-key provenance: one `(merged VALUE node, host mapping node)` pair per entry a `<<:` splice admitted. The
    /// merged entry REUSES the anchored mapping's node ids, so without this ledger the document cannot tell a
    /// merge-inherited member from a host member — the edit lane uses it to splice an override into the host instead of
    /// patching the anchor.
    merge_hosts: Vec<(NodeId, NodeId)>,
}

impl YamlGraph {
    /// An EMPTY placeholder graph for swap-out (never scanned; the session rejects multi-document streams before a
    /// second parse).
    pub(crate) fn try_new_placeholder() -> Self {
        Self {
            nodes: Vec::new(),
            root: None,
            anchors: Vec::new(),
            names: Vec::new(),
            name_ids: BTreeMap::new(),
            texts: Vec::new(),
            children: Vec::new(),
            entries: Vec::new(),
            comments: Vec::new(),
            merge_hosts: Vec::new(),
        }
    }

    pub(crate) fn try_new() -> Result<Self, jqf_resource::ResourceError> {
        fn reserved<T>(n: usize) -> Result<Vec<T>, jqf_resource::ResourceError> {
            let mut v = Vec::new();
            v.try_reserve_exact(n)?;
            Ok(v)
        }
        Ok(Self {
            nodes: reserved(16)?,
            root: None,
            anchors: reserved(4)?,
            names: reserved(4)?,
            name_ids: BTreeMap::new(),
            texts: reserved(4)?,
            children: reserved(8)?,
            entries: reserved(8)?,
            comments: reserved(4)?,
            merge_hosts: Vec::new(),
        })
    }

    /// Appends a scalar node.
    ///
    /// Refuses a document past what a node id can address (`Overflow`) rather than saturating: every node past 2^32
    /// sharing one id would corrupt memo/alias/cycle detection silently.
    pub(crate) fn add_scalar(
        &mut self,
        text: TextRef,
        style: u8,
        tag: Option<u32>,
        anchor: Option<u32>,
        span: Span,
    ) -> Result<NodeId, CodecError> {
        let id = self.next_node_id()?;
        self.nodes.push(NodeRecord::Scalar {
            text,
            style,
            props: NodeProps::encode(tag, anchor),
            span,
        });
        Ok(id)
    }

    /// Appends a sequence node; its item range is filled at [`Self::close_sequence`].
    pub(crate) fn add_sequence(
        &mut self,
        tag: Option<u32>,
        anchor: Option<u32>,
        span: Span,
    ) -> Result<NodeId, CodecError> {
        let id = self.next_node_id()?;
        let start = flat_arena_start(self.children.len())?;
        self.nodes.push(NodeRecord::Sequence {
            range: (start, 0),
            props: NodeProps::encode(tag, anchor),
            span,
        });
        Ok(id)
    }

    /// Appends a mapping node; its entry range is filled at [`Self::close_mapping`].
    pub(crate) fn add_mapping(
        &mut self,
        tag: Option<u32>,
        anchor: Option<u32>,
        span: Span,
    ) -> Result<NodeId, CodecError> {
        let id = self.next_node_id()?;
        let start = flat_arena_start(self.entries.len())?;
        self.nodes.push(NodeRecord::Mapping {
            range: (start, 0),
            props: NodeProps::encode(tag, anchor),
            span,
        });
        Ok(id)
    }

    /// Returns the source-ordered merge provenance pairs: the merged VALUE node and the host mapping it spliced into.
    pub(crate) fn merge_hosts(&self) -> &[(NodeId, NodeId)] {
        self.merge_hosts.as_slice()
    }

    /// Records one merge provenance pair.
    pub(crate) fn record_merge_host(&mut self, value: NodeId, host: NodeId) {
        self.merge_hosts.push((value, host));
    }

    /// Appends an alias occurrence.
    pub(crate) fn add_alias(&mut self, target: NodeId, span: Span) -> Result<NodeId, CodecError> {
        let id = self.next_node_id()?;
        self.nodes.push(NodeRecord::Alias(target, span));
        Ok(id)
    }

    /// The next node id: the arena length, refused (`Overflow`) when it exceeds what a node id can carry. A saturated
    /// id would alias every later node onto one slot, corrupting memo/alias/cycle detection.
    fn next_node_id(&self) -> Result<NodeId, CodecError> {
        let len = self.nodes.as_slice().len();
        u32::try_from(len)
            .map(NodeId)
            .map_err(|_| CodecError::new(CodecFailureKind::Overflow))
    }

    /// The target of an alias record (callers check the kind first).
    #[must_use]
    pub(crate) fn alias_target(&self, id: NodeId) -> NodeId {
        match self.nodes.as_slice().get(id.index()) {
            Some(NodeRecord::Alias(target, _)) => *target,
            _ => NodeId(u32::MAX),
        }
    }

    /// The end of a node's whole source subtree: a scalar's content end, an alias token's end, or — descending the
    /// LAST-entry chain of a collection — the last member's subtree end. This is the byte the edit lane's container
    /// spans close on: a block mapping's recorded span ends where its last member's content ends, whatever that member
    /// is. The descent is ITERATIVE: the standing stack-depth gate pins the YAML document build at 24257 KiB with a
    /// 10000-deep document, and a per-level recursion here would add a second 10000 frames on top of the walker's own
    /// guarded recursion.
    #[must_use]
    pub(crate) fn subtree_end(&self, id: NodeId) -> u32 {
        let mut current = id;
        loop {
            match self.nodes.as_slice().get(current.index()) {
                Some(NodeRecord::Scalar { span, .. } | NodeRecord::Alias(_, span)) => {
                    return span.end();
                }
                Some(NodeRecord::Sequence { range, .. }) => {
                    let (start, len) = *range;
                    match last_of(self.children.as_slice(), start, len) {
                        Some(last) => current = *last,
                        None => return self.node_span(current).end(),
                    }
                }
                Some(NodeRecord::Mapping { range, .. }) => {
                    let (start, len) = *range;
                    match last_of(self.entries.as_slice(), start, len) {
                        Some((_, last_value)) => current = *last_value,
                        None => return self.node_span(current).end(),
                    }
                }
                None => return 0,
            }
        }
    }

    /// Flushes a closed sequence's buffered items into the flat arena and records its range. Called at collection close
    /// so every collection's range is contiguous in the arena (closure order is source order).
    pub(crate) fn close_sequence(&mut self, seq: NodeId, items: &[NodeId]) -> Result<(), jqf_resource::ResourceError> {
        self.children.try_reserve(items.len())?;
        let start = u32::try_from(self.children.len()).unwrap_or(u32::MAX);
        for item in items {
            self.children.push(*item);
        }
        match &mut self.nodes.as_mut_slice()[seq.index()] {
            NodeRecord::Sequence { range, .. } => {
                *range = (start, u32::try_from(items.len()).unwrap_or(u32::MAX));
            }
            _ => return Err(jqf_resource::ResourceError::AccountingInvariantViolation),
        }
        Ok(())
    }

    /// Flushes a closed mapping's buffered entries into the flat arena and records its range.
    pub(crate) fn close_mapping(
        &mut self,
        map: NodeId,
        entries: &[(NodeId, NodeId)],
    ) -> Result<(), jqf_resource::ResourceError> {
        self.entries.try_reserve(entries.len())?;
        let start = u32::try_from(self.entries.len()).unwrap_or(u32::MAX);
        for entry in entries {
            self.entries.push(*entry);
        }
        match &mut self.nodes.as_mut_slice()[map.index()] {
            NodeRecord::Mapping { range, .. } => {
                *range = (start, u32::try_from(entries.len()).unwrap_or(u32::MAX));
            }
            _ => return Err(jqf_resource::ResourceError::AccountingInvariantViolation),
        }
        Ok(())
    }

    /// Interns a name (anchor or tag text), returning its arena index.
    ///
    /// Refuses a document whose distinct tag/anchor names exceed what the packed node record can address
    /// ([`MAX_NAME_ID`]): minting a past-bound id would encode as a DIFFERENT name's slot, mis-tagging every node that
    /// carries it.
    pub(crate) fn intern_name(&mut self, name: &str) -> Result<u32, CodecError> {
        if let Some(&id) = self.name_ids.get(name) {
            return Ok(id);
        }
        let id = u32::try_from(self.names.len()).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?;
        if id > MAX_NAME_ID {
            return Err(CodecError::new(CodecFailureKind::Overflow));
        }
        self.names.push(name.to_owned());
        let key: Box<str> = name.into();
        self.name_ids.insert(key, id);
        Ok(id)
    }

    /// Returns the interned name text.
    #[must_use]
    pub(crate) fn name(&self, id: u32) -> &str {
        self.names.as_slice()[usize::try_from(id).unwrap_or(usize::MAX)].as_str()
    }

    /// Stores an owned scalar text, returning its arena index.
    pub(crate) fn store_text(&mut self, text: &str) -> u32 {
        let id = u32::try_from(self.texts.len()).unwrap_or(u32::MAX);
        let boxed: Box<str> = text.into();
        self.texts.push(boxed);
        id
    }

    /// Binds an anchor name to a node (source-ordered history per document).
    pub(crate) fn bind_anchor(&mut self, name: u32, node: NodeId) {
        self.anchors.push(AnchorBinding { name, node });
    }

    /// Resolves an alias: the MOST RECENT preceding anchor with the name. A forward alias returns `None` (the caller
    /// raises the typed error).
    #[must_use]
    pub(crate) fn resolve_alias(&self, name: u32) -> Option<NodeId> {
        self.anchors
            .as_slice()
            .iter()
            .rev()
            .find(|binding| binding.name == name)
            .map(|binding| binding.node)
    }

    /// The document root node, recorded by the parser.
    #[must_use]
    pub(crate) fn root(&self) -> Option<NodeId> {
        self.root
    }

    /// Records the document root (the parser calls this when a node closes with no open parent).
    pub(crate) fn set_root(&mut self, root: NodeId) {
        self.root = Some(root);
    }

    /// The kind of one node (no source needed; alias chains resolve by kind).
    #[must_use]
    pub(crate) fn node_kind(&self, id: NodeId) -> Option<NodeKind> {
        match self.nodes.as_slice().get(id.index()) {
            Some(NodeRecord::Scalar { .. }) => Some(NodeKind::Scalar),
            Some(NodeRecord::Sequence { .. }) => Some(NodeKind::Sequence),
            Some(NodeRecord::Mapping { .. }) => Some(NodeKind::Mapping),
            Some(NodeRecord::Alias(..)) => Some(NodeKind::Alias),
            None => None,
        }
    }

    /// The source span of a node's content (an alias's token span).
    #[must_use]
    pub(crate) fn node_span(&self, id: NodeId) -> Span {
        match self.nodes.as_slice().get(id.index()) {
            Some(
                NodeRecord::Scalar { span, .. }
                | NodeRecord::Sequence { span, .. }
                | NodeRecord::Mapping { span, .. }
                | NodeRecord::Alias(_, span),
            ) => *span,
            _ => Span::try_from_usize(0, 0).unwrap_or_else(|_| unreachable!("zero span")),
        }
    }

    /// Appends one comment's source span (source order; the scanner records trivia spans as it skips them).
    pub(crate) fn add_comment(&mut self, span: Span) {
        self.comments.push(span);
    }

    /// Every recorded comment span, in source order.
    #[must_use]
    pub(crate) fn comments(&self) -> &[Span] {
        self.comments.as_slice()
    }

    /// Every non-alias node's `(id, span.start, span.end)`, for the comment-to-node association pass.
    #[must_use]
    pub(crate) fn node_span_pairs(&self) -> Vec<(NodeId, u32, u32)> {
        self.nodes
            .as_slice()
            .iter()
            .enumerate()
            .filter_map(|(index, record)| match record {
                NodeRecord::Scalar { span, .. }
                | NodeRecord::Sequence { span, .. }
                | NodeRecord::Mapping { span, .. } => Some((
                    NodeId(u32::try_from(index).unwrap_or(u32::MAX)),
                    span.start(),
                    span.end(),
                )),
                NodeRecord::Alias(_, _) => None,
            })
            .collect()
    }

    /// Every graph node an alias occurrence references, deduplicated. The document walk shares ONE document node across
    /// an anchor and its aliases, so these are the document nodes whose authored span is ambiguous to patch — the edit
    /// lane's alias-refusal set.
    #[must_use]
    pub(crate) fn alias_targets(&self) -> Vec<NodeId> {
        let mut marked = alloc::vec![false; self.len()];
        let mut seen = alloc::vec::Vec::new();
        for record in self.nodes.as_slice() {
            if let NodeRecord::Alias(target, _) = record {
                let index = target.index();
                if index < marked.len() && !marked[index] {
                    marked[index] = true;
                    seen.push(*target);
                }
            }
        }
        seen
    }

    /// Every node in the subtree rooted at `root`, the root included, deduplicated. A merge key (`<<: *anchor`) splices
    /// the anchored mapping's entries into the host mapping BY REUSING the source node ids, so the merged-in members
    /// are DESCENDANTS of the alias target — the refusal set must cover them. The walk is ITERATIVE (the standing
    /// stack-depth gate forbids a second per-level recursion on top of the parser's guarded one) and descends mapping
    /// entries — keys and values alike — and sequence items; an alias record is a leaf, because its target is a
    /// separate subtree, not a descendant.
    #[must_use]
    pub(crate) fn subtree_nodes(&self, root: NodeId) -> Vec<NodeId> {
        let mut marked = alloc::vec![false; self.len()];
        if root.index() < marked.len() {
            marked[root.index()] = true;
        }
        let mut seen = alloc::vec![root];
        let mut pending = alloc::vec![root];
        while let Some(node) = pending.pop() {
            let children = match self.nodes.as_slice().get(node.index()) {
                Some(NodeRecord::Mapping { range, .. }) => {
                    let (start, len) = *range;
                    let mut children = alloc::vec::Vec::new();
                    for (key, value) in self.entries.as_slice().iter().skip(start as usize).take(len as usize) {
                        children.push(*key);
                        children.push(*value);
                    }
                    children
                }
                Some(NodeRecord::Sequence { range, .. }) => {
                    let (start, len) = *range;
                    self.children
                        .as_slice()
                        .iter()
                        .skip(start as usize)
                        .take(len as usize)
                        .copied()
                        .collect()
                }
                _ => alloc::vec::Vec::new(),
            };
            for child in children {
                let index = child.index();
                if index < marked.len() && !marked[index] {
                    marked[index] = true;
                    seen.push(child);
                    pending.push(child);
                }
            }
        }
        seen
    }

    /// Every mapping `(key, value)` pair across the whole graph, for the comment-to-value redirect (a leading comment
    /// before a key must attach to the key's VALUE's document node, because `.key` resolves there).
    #[must_use]
    pub(crate) fn key_value_pairs(&self) -> Vec<(NodeId, NodeId)> {
        let mut out = Vec::new();
        for record in self.nodes.as_slice() {
            if let NodeRecord::Mapping { range, .. } = record {
                let (start, len) = *range;
                for pair in self.entries.as_slice().iter().skip(start as usize).take(len as usize) {
                    out.push(*pair);
                }
            }
        }
        out
    }

    /// One parent COLLECTION per node, indexed by graph node index (the foot-owner walk): the mapping or sequence whose
    /// entry or item directly contains the node. The root has no parent. Derivable once per graph; the
    /// comment-association pass reuses it across every per graph; the comment-association pass reuses it across scoped
    /// builds like the other graph-derived lookup tables.
    #[must_use]
    pub(crate) fn parent_collections(&self) -> Vec<Option<NodeId>> {
        let mut parent = alloc::vec![None; self.len()];
        for (index, record) in self.nodes.as_slice().iter().enumerate() {
            let collection = NodeId(u32::try_from(index).unwrap_or(u32::MAX));
            match record {
                NodeRecord::Mapping { range, .. } => {
                    let (start, len) = *range;
                    for (key, value) in self.entries.as_slice().iter().skip(start as usize).take(len as usize) {
                        parent[key.index()] = Some(collection);
                        parent[value.index()] = Some(collection);
                    }
                }
                NodeRecord::Sequence { range, .. } => {
                    let (start, len) = *range;
                    for item in self.children.as_slice().iter().skip(start as usize).take(len as usize) {
                        parent[item.index()] = Some(collection);
                    }
                }
                _ => {}
            }
        }
        parent
    }

    /// Materializes the borrowed view of one node, resolving its text from the source or the owned-text arena and its
    /// properties from the name arena. A missing node is an internal contract violation.
    pub(crate) fn node<'a>(&'a self, id: NodeId, source: jqf_source::ResolvedSource<'a>) -> YamlNode<'a> {
        // The caller must have validated the id; a missing record is a broken walk, not a recoverable observation.
        self.node_opt(id, source)
            .expect("YAML graph node missing during a walk")
    }

    /// The graph's total node count (the memo and alias tables are sized from it).
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.nodes.as_slice().len()
    }

    /// Follows alias nodes to their resolved target. A cycle is `UnsupportedRepresentation` (the graph retains it; a
    /// semantic value cannot).
    pub(crate) fn follow_alias(&self, mut id: NodeId, source: ResolvedSource<'_>) -> Result<NodeId, CodecError> {
        let mut hops = 0usize;
        while let Some(YamlNode::Alias(target)) = self.node_opt(id, source) {
            hops += 1;
            if hops > self.len() {
                return Err(CodecError::new(CodecFailureKind::UnsupportedRepresentation));
            }
            id = target;
        }
        Ok(id)
    }

    /// Materializes the borrowed view of one node, or `None` when the id is out of range.
    #[must_use]
    #[inline]
    pub(crate) fn node_opt<'a>(&'a self, id: NodeId, source: jqf_source::ResolvedSource<'a>) -> Option<YamlNode<'a>> {
        let record = *self.nodes.as_slice().get(id.index())?;
        Some(match record {
            NodeRecord::Scalar {
                text,
                style,
                props,
                span,
            } => {
                let props = self.props(props);
                YamlNode::Scalar {
                    text: self.text(text, source),
                    style: ScalarStyle::from_u8(style),
                    tag: props.0,
                    anchor: props.1,
                    span,
                }
            }
            NodeRecord::Sequence { range, props, span } => {
                let props = self.props(props);
                YamlNode::Sequence {
                    items: self.children.as_slice().get(range_slice(range))?,
                    tag: props.0,
                    anchor: props.1,
                    span,
                }
            }
            NodeRecord::Mapping { range, props, span } => {
                let props = self.props(props);
                YamlNode::Mapping {
                    entries: self.entries.as_slice().get(range_slice(range))?,
                    tag: props.0,
                    anchor: props.1,
                    span,
                }
            }
            NodeRecord::Alias(target, _) => YamlNode::Alias(target),
        })
    }

    /// Resolves a text reference to its `&str` (from the source for a span, from the owned-text arena otherwise).
    fn text<'a>(&'a self, text: TextRef, source: jqf_source::ResolvedSource<'a>) -> &'a str {
        match text {
            TextRef::Span(span) => source
                .bytes()
                .get(span.start() as usize..span.end() as usize)
                .and_then(|bytes| core::str::from_utf8(bytes).ok())
                .unwrap_or(""),
            TextRef::Owned(index) => self.texts.as_slice()[usize::try_from(index).unwrap_or(usize::MAX)].as_ref(),
        }
    }

    /// Decodes packed property ids into the interned name texts. The zero case (no properties) is the hot path and
    /// exits before any lookup.
    #[inline]
    fn props(&self, props: u32) -> (Option<&str>, Option<&str>) {
        if props == 0 {
            return (None, None);
        }
        let props = NodeProps {
            tag: (props & 0xffff) as u16,
            anchor: ((props >> 16) & 0xffff) as u16,
        };
        let tag = props.tag_id().map(|id| self.name(id));
        let anchor = props.anchor_id().map(|id| self.name(id));
        (tag, anchor)
    }
}

/// Converts a `[start, len)` range into a `start..end` slice index, clamped.
fn range_slice(range: (u32, u32)) -> core::ops::Range<usize> {
    let start = usize::try_from(range.0).unwrap_or(usize::MAX);
    let len = usize::try_from(range.1).unwrap_or(usize::MAX);
    start..start.saturating_add(len)
}

/// The flat-arena start a new collection records: refused (`Overflow`) when the arena length exceeds what its packed
/// range can carry, never saturated.
fn flat_arena_start(len: usize) -> Result<u32, CodecError> {
    u32::try_from(len).map_err(|_| CodecError::new(CodecFailureKind::Overflow))
}

/// The LAST element of a flat arena range, when the range is non-empty.
fn last_of<T>(arena: &[T], start: u32, len: u32) -> Option<&T> {
    if len == 0 {
        return None;
    }
    arena.get(usize::try_from(start).unwrap_or(usize::MAX) + (len as usize) - 1)
}

#[cfg(test)]
mod perf_ab {
    use super::*;
    use std::time::Instant;

    const N: usize = 4_000;
    const ROUNDS: usize = 15;

    fn span() -> Span {
        Span::try_from_usize(0, 1).expect("span")
    }

    fn wide_sequence() -> (YamlGraph, NodeId) {
        let mut graph = YamlGraph::try_new().expect("graph");
        let seq = graph.add_sequence(None, None, span()).expect("seq");
        let mut items = Vec::with_capacity(N);
        for _ in 0..N {
            let text = graph.store_text("x");
            items.push(
                graph
                    .add_scalar(TextRef::Owned(text), 0, None, None, span())
                    .expect("scalar"),
            );
        }
        graph.close_sequence(seq, &items).expect("close");
        let _alias = graph.add_alias(seq, span()).expect("alias");
        (graph, seq)
    }

    fn subtree_nodes_contains(graph: &YamlGraph, root: NodeId) -> Vec<NodeId> {
        let mut seen = alloc::vec![root];
        let mut pending = alloc::vec![root];
        while let Some(node) = pending.pop() {
            let children = match graph.nodes.as_slice().get(node.index()) {
                Some(NodeRecord::Sequence { range, .. }) => {
                    let (start, len) = *range;
                    graph
                        .children
                        .as_slice()
                        .iter()
                        .skip(start as usize)
                        .take(len as usize)
                        .copied()
                        .collect::<Vec<_>>()
                }
                _ => Vec::new(),
            };
            for child in children {
                if !seen.contains(&child) {
                    seen.push(child);
                    pending.push(child);
                }
            }
        }
        seen
    }

    fn median(mut samples: Vec<u128>) -> u128 {
        samples.sort_unstable();
        samples[samples.len() / 2]
    }

    #[test]
    fn alias_walk_bitmap_vs_contains() {
        let (graph, root) = wide_sequence();
        let mut old_samples = Vec::with_capacity(ROUNDS);
        let mut new_samples = Vec::with_capacity(ROUNDS);
        for _ in 0..ROUNDS {
            let start = Instant::now();
            let old = subtree_nodes_contains(&graph, root);
            old_samples.push(start.elapsed().as_nanos());
            let start = Instant::now();
            let new = graph.subtree_nodes(root);
            new_samples.push(start.elapsed().as_nanos());
            assert_eq!(old.len(), new.len());
        }
        let old_med = median(old_samples);
        let new_med = median(new_samples);
        eprintln!("lane=subtree_nodes n={N} rounds={ROUNDS} contains_ns={old_med} bitmap_ns={new_med}");
        assert!(new_med < old_med, "bitmap must beat Vec::contains on this width");
    }
}
