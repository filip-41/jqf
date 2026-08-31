//! The per-evaluation markup index: one owned walk of the format-neutral document, built once per selector activation.
//!
//! The seam evaluates over the portable footprint — the recovered [`jqf_data::Document`] — so an activation indexes
//! the document once (topology for the ordered element children, facts for the name/attrs/ content roles) and then
//! matches against the owned index. Every string copied here is bounded by the activation's walk budget, and every
//! allocation is checked against the request ledger.

use alloc::vec;
use alloc::vec::Vec;
use jqf_data::{Document, FactPayloadView, LocalOwnerRef, NodeId, ReaderPoll, unbounded_batch_limit};
use jqf_resource::ResourceContext;

use jqf_codec_core::markup;

use super::lang::SelectorLanguage;
use super::{SelectorBudget, SelectorError};

/// Leaf classification built once from interned kind segments.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LeafSort {
    /// Name fact present.
    Element,
    /// Kernel `text` kind.
    Text,
    /// Kernel `comment` kind.
    Comment,
    /// Kernel `pi` kind.
    Pi,
}

/// One node reference during evaluation: the document node or an element.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NodeRef {
    /// The virtual document node (the parent of the document element; the start of absolute XPath paths). Never an
    /// element; never a result.
    Document,
    /// One element node id.
    Element(NodeId),
}

impl NodeRef {
    #[must_use]
    pub(crate) const fn element(self) -> Option<NodeId> {
        match self {
            Self::Element(node) => Some(node),
            Self::Document => None,
        }
    }
}

/// The owned markup walk.
pub(crate) struct MarkupIndex {
    /// Per node: whether it is an element (carries the name fact).
    pub elements: Vec<bool>,
    /// Per node: the dense element ordinal (for packing the element vectors).
    pub ordinal: Vec<u32>,
    /// Per ELEMENT: ordered child node ids (text leaves and child elements).
    pub children: Vec<Vec<NodeId>>,
    /// Per ELEMENT: its parent element (the document element has none).
    pub parent: Vec<Option<NodeId>>,
    /// Per node: its index within its parent element's ordered children vector. Meaningful exactly when
    /// [`Self::parent`] is `Some`; filled at topology-build time so sibling steps never rescan the children vector to
    /// rediscover it.
    pub sibling_pos: Vec<u32>,
    /// Per node: how many ELEMENT siblings precede it under its parent.
    /// Meaningful exactly when [`Self::parent`] is `Some`.
    pub elem_sibs_before: Vec<u32>,
    /// Per ELEMENT: how many ELEMENT children it has (0 for every leaf).
    pub elem_child_count: Vec<u32>,
    /// Per ELEMENT: the normalized expanded name.
    pub names: Vec<alloc::string::String>,
    /// Per ELEMENT: the semantic attribute map, in recovered order.
    pub attrs: Vec<Vec<(alloc::string::String, alloc::string::String)>>,
    /// Per ELEMENT: the textContent fact (concatenated descendant text).
    pub content: Vec<alloc::string::String>,
    /// Per ELEMENT: the pre-order (document-order) rank over all elements.
    pub rank: Vec<u32>,
    /// Per node: leaf sort from the interned kind segment.
    pub leaf: Vec<LeafSort>,
    /// Per node: the string content of a TEXT leaf (empty for every other node kind). Copied for the same reason.
    pub leaf_text: Vec<alloc::string::String>,
    /// The document element (the root of the element tree).
    pub document_element: NodeId,
    /// The document's pragma-set default language (HTML `html.css@1`), when the document carries it as a document-level
    /// fact.
    pub pragma_language: Option<alloc::string::String>,
    /// The recovered document mode text (`no-quirks`/`limited-quirks`/ `quirks`) when the document carries it.
    pub mode: Option<alloc::string::String>,
}

impl MarkupIndex {
    /// Builds the index over one document.
    ///
    /// `roles` are the language's fact roles. The index-build budget is drawn from `budget.max_walk_steps` so a hostile
    /// document cannot win an unbounded index phase either.
    pub(crate) fn build(
        document: &Document<'_>,
        language: SelectorLanguage,
        budget: SelectorBudget,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Self, SelectorError> {
        let roles = language.fact_roles();
        let node_count = document.node_count();
        let mut steps = 0u64;
        let charge = |steps: &mut u64| -> Result<(), SelectorError> {
            let next = steps
                .checked_add(1)
                .ok_or_else(|| SelectorError::Budget { what: "walk steps" })?;
            if next > budget.max_walk_steps {
                return Err(SelectorError::Budget { what: "walk steps" });
            }
            *steps = next;
            Ok(())
        };

        let limit = unbounded_batch_limit();
        let mut name_by_node: Vec<Option<alloc::string::String>> = vec![None; node_count];
        let mut attrs_by_node: Vec<Option<Vec<(alloc::string::String, alloc::string::String)>>> =
            vec![None; node_count];
        let mut content_by_node: Vec<Option<alloc::string::String>> = vec![None; node_count];
        // Document-level facts the languages may consult (the HTML mode and
        // pragma-set default language) ride the SAME fact scan — a second full
        // table pass per selection doubled the walk for nothing. The codec
        // attaches them to the document element, whose identity is only known
        // after the topology pass, so candidates are captured per owner and
        // matched against it below.
        let mut pragma_candidate: Option<(usize, alloc::string::String)> = None;
        let mut mode_candidate: Option<(usize, alloc::string::String)> = None;

        // Facts: name/attrs/content roles identify elements and their surface.
        let mut reader = document.fact_reader(resources).map_err(|_| SelectorError::NotMarkup)?;
        loop {
            match reader.poll_batch(limit, resources).map_err(super::map_data)? {
                ReaderPoll::Batch(batch) => {
                    for fact in batch.iter() {
                        let LocalOwnerRef::Node(owner) = fact.owner() else {
                            continue;
                        };
                        let index = node_index(owner, node_count);
                        let role = fact.role().as_str();
                        let kind = fact.kind().as_str();
                        if role == roles.name {
                            if let FactPayloadView::Text(text) = fact.payload() {
                                charge(&mut steps)?;
                                name_by_node[index] = Some(alloc::string::String::from(text));
                            }
                        } else if role == roles.attrs {
                            if let FactPayloadView::Map(map) = fact.payload() {
                                charge(&mut steps)?;
                                let mut pairs = Vec::new();
                                for (key, value) in map.iter() {
                                    if let FactPayloadView::Text(text) = value {
                                        charge(&mut steps)?;
                                        pairs.push((
                                            alloc::string::String::from(key),
                                            alloc::string::String::from(text),
                                        ));
                                    }
                                }
                                attrs_by_node[index] = Some(pairs);
                            }
                        } else if role == roles.content {
                            if let FactPayloadView::Text(text) = fact.payload() {
                                charge(&mut steps)?;
                                content_by_node[index] = Some(alloc::string::String::from(text));
                            }
                        } else if role == roles.attribute {
                            // The per-attribute `.&` facts fill gaps when the semantic map fact is absent: one fact per
                            // attribute, kind = attribute name. A map payload carries a recovered name that is not a
                            // fact identity (HTML control-byte names).
                            match fact.payload() {
                                FactPayloadView::Text(text) => {
                                    charge(&mut steps)?;
                                    let entry = attrs_by_node[index].get_or_insert_with(Vec::new);
                                    entry.push((alloc::string::String::from(kind), alloc::string::String::from(text)));
                                }
                                FactPayloadView::Map(map) => {
                                    let mut name = None;
                                    let mut value = None;
                                    for (key, payload) in map.iter() {
                                        match (key, payload) {
                                            ("name", FactPayloadView::Text(text)) => {
                                                name = Some(alloc::string::String::from(text));
                                            }
                                            ("value", FactPayloadView::Text(text)) => {
                                                value = Some(alloc::string::String::from(text));
                                            }
                                            _ => {}
                                        }
                                    }
                                    if let (Some(name), Some(value)) = (name, value) {
                                        charge(&mut steps)?;
                                        let entry = attrs_by_node[index].get_or_insert_with(Vec::new);
                                        entry.push((name, value));
                                    }
                                }
                                _ => {}
                            }
                        } else if language.pragma_language_role() == Some(role) || language.mode_role() == Some(role) {
                            if let FactPayloadView::Text(text) = fact.payload() {
                                charge(&mut steps)?;
                                let slot = if language.pragma_language_role() == Some(role) {
                                    &mut pragma_candidate
                                } else {
                                    &mut mode_candidate
                                };
                                *slot = Some((index, alloc::string::String::from(text)));
                            }
                        }
                    }
                }
                ReaderPoll::Pending => {
                    resources
                        .try_begin_next_cooperative_entry(4_096)
                        .map_err(SelectorError::from)?;
                }
                ReaderPoll::End(_) => break,
            }
        }

        // Elements are exactly the nodes with a name fact; every other node is a leaf (text, comment, pi) with no
        // children.
        let mut elements = vec![false; node_count];
        let mut ordinal = vec![0u32; node_count];
        let mut element_ids: Vec<NodeId> = Vec::new();
        let mut element_count = 0u32;
        for (index, name) in name_by_node.iter().enumerate() {
            if name.is_some() {
                elements[index] = true;
                ordinal[index] = element_count;
                element_count = element_count.checked_add(1).ok_or_else(|| SelectorError::Internal {
                    contract: "element count",
                })?;
                element_ids.push(NodeId::try_from_index(index).ok_or_else(|| SelectorError::Internal {
                    contract: "node id overflow",
                })?);
            }
        }
        if element_ids.is_empty() {
            return Err(SelectorError::NotMarkup);
        }

        // Topology: ordered children per element, node kinds, and leaf text.
        let mut children: Vec<Vec<NodeId>> = vec![Vec::new(); node_count];
        let mut parent: Vec<Option<NodeId>> = vec![None; node_count];
        let mut sibling_pos: Vec<u32> = vec![0; node_count];
        let mut leaf = vec![LeafSort::Element; node_count];
        let mut leaf_text: Vec<alloc::string::String> = vec![alloc::string::String::new(); node_count];
        let mut reader = document.topology_reader(resources).map_err(super::map_data)?;
        loop {
            match reader.poll_batch(limit, resources).map_err(super::map_data)? {
                ReaderPoll::Batch(batch) => match batch {
                    jqf_data::TopologyBatch::Nodes(nodes) => {
                        for view in nodes.iter() {
                            let view = view.map_err(super::map_data)?;
                            let index = node_index(view.id(), node_count);
                            charge(&mut steps)?;
                            let kind = view.kind().as_str();
                            leaf[index] = if elements[index] {
                                LeafSort::Element
                            } else if kind == markup::TEXT_KIND {
                                LeafSort::Text
                            } else if kind == markup::COMMENT_KIND {
                                LeafSort::Comment
                            } else if kind == markup::PI_KIND {
                                LeafSort::Pi
                            } else {
                                LeafSort::Element
                            };
                            if let Some(semantic) = view.semantic() {
                                if let Some(scalar) = semantic.scalar().map_err(super::map_data)? {
                                    if let jqf_data::ScalarView::String(text) = scalar {
                                        charge(&mut steps)?;
                                        leaf_text[index] = alloc::string::String::from(text);
                                    }
                                }
                            }
                        }
                    }
                    jqf_data::TopologyBatch::Occurrences(occurrences) => {
                        for occurrence in occurrences.iter() {
                            let occurrence = occurrence.map_err(super::map_data)?;
                            let LocalOwnerRef::Node(owner) = occurrence.owner() else {
                                continue;
                            };
                            let owner_index = node_index(owner, node_count);
                            if !elements[owner_index] {
                                continue;
                            }
                            charge(&mut steps)?;
                            let target = occurrence.target();
                            let target_index = node_index(target, node_count);
                            // The push below fixes this child's ordinal in its parent's children vector for good: the
                            // lists are appended to, never spliced.
                            sibling_pos[target_index] =
                                u32::try_from(children[owner_index].len()).map_err(|_| SelectorError::Internal {
                                    contract: "children vector overflow",
                                })?;
                            children[owner_index].push(target);
                            if elements[target_index] && parent[target_index].is_none() {
                                parent[target_index] = Some(owner);
                            }
                        }
                    }
                },
                ReaderPoll::Pending => {
                    resources
                        .try_begin_next_cooperative_entry(4_096)
                        .map_err(SelectorError::from)?;
                }
                ReaderPoll::End(_) => break,
            }
        }

        // Sibling ordinals over the completed children lists: per element child, its rank among ELEMENT siblings, and
        // per element its ELEMENT-child count. One linear pass over exactly the child entries the topology walk already
        // charged for; deliberately uncharged so a build's charged-step stream is unchanged.
        let mut elem_sibs_before: Vec<u32> = vec![0; node_count];
        let mut elem_child_count: Vec<u32> = vec![0; node_count];
        for owner_index in 0..node_count {
            if !elements[owner_index] {
                continue;
            }
            let mut element_run = 0u32;
            for child in &children[owner_index] {
                let child_index = node_index(*child, node_count);
                if elements[child_index] {
                    elem_sibs_before[child_index] = element_run;
                    element_run = element_run.checked_add(1).ok_or_else(|| SelectorError::Internal {
                        contract: "element count",
                    })?;
                }
            }
            elem_child_count[owner_index] = element_run;
        }

        // The document element: the single element with no parent.
        let mut document_element = None;
        let mut root_count = 0usize;
        for id in &element_ids {
            if parent[node_index(*id, node_count)].is_none() {
                root_count += 1;
                if document_element.is_none() {
                    document_element = Some(*id);
                }
            }
        }
        if root_count > 1 {
            return Err(SelectorError::Internal {
                contract: "single-root element tree",
            });
        }
        let document_element = document_element.ok_or_else(|| SelectorError::Internal {
            contract: "element tree without a root",
        })?;

        // Pre-order ranks over the element tree (document order).
        let mut rank = vec![0u32; node_count];
        let mut next_rank = 0u32;
        let mut stack = alloc::vec![document_element];
        while let Some(node) = stack.pop() {
            let index = node_index(node, node_count);
            rank[index] = next_rank;
            next_rank = next_rank.checked_add(1).ok_or_else(|| SelectorError::Internal {
                contract: "rank overflow",
            })?;
            // Children in reverse so the pop order is document order.
            for child in children[index].iter().rev() {
                if elements[node_index(*child, node_count)] {
                    stack.push(*child);
                }
            }
        }

        // Pack the element vectors in node order.
        let mut names = Vec::new();
        let mut attrs = Vec::new();
        let mut content = Vec::new();
        for index in 0..node_count {
            if !elements[index] {
                continue;
            }
            let name = name_by_node[index].take().ok_or_else(|| SelectorError::Internal {
                contract: "element without a name fact",
            })?;
            names.push(name);
            attrs.push(attrs_by_node[index].take().unwrap_or_default());
            content.push(content_by_node[index].take().unwrap_or_default());
        }

        // The captured document-level candidates only count when their owner
        // IS the document element.
        let document_element_index = node_index(document_element, node_count);
        let pragma_language = match pragma_candidate {
            Some((index, text)) if index == document_element_index => Some(text),
            _ => None,
        };
        let mode = match mode_candidate {
            Some((index, text)) if index == document_element_index => Some(text),
            _ => None,
        };

        Ok(Self {
            elements,
            ordinal,
            children,
            parent,
            sibling_pos,
            elem_sibs_before,
            elem_child_count,
            names,
            attrs,
            content,
            rank,
            leaf,
            leaf_text,
            document_element,
            pragma_language,
            mode,
        })
    }

    /// Whether `node` is an element.
    #[inline]
    pub(crate) fn is_element(&self, node: NodeId) -> bool {
        self.elements[node_index(node, self.elements.len())]
    }

    /// The element's name (empty when not an element).
    #[inline]
    pub(crate) fn name_of(&self, node: NodeId) -> &str {
        let index = node_index(node, self.elements.len());
        if self.elements[index] {
            &self.names[self.ordinal[index] as usize]
        } else {
            ""
        }
    }

    /// The element's ordered semantic attributes.
    #[inline]
    pub(crate) fn attrs_of(&self, node: NodeId) -> &[(alloc::string::String, alloc::string::String)] {
        let index = node_index(node, self.elements.len());
        if self.elements[index] {
            &self.attrs[self.ordinal[index] as usize]
        } else {
            &[]
        }
    }

    /// One attribute value by exact name.
    #[inline]
    pub(crate) fn attr(&self, node: NodeId, name: &str) -> Option<&str> {
        self.attrs_of(node)
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The element's textContent.
    #[inline]
    pub(crate) fn content_of(&self, node: NodeId) -> &str {
        let index = node_index(node, self.elements.len());
        if self.elements[index] {
            &self.content[self.ordinal[index] as usize]
        } else {
            ""
        }
    }

    /// The element's ordered children.
    #[inline]
    pub(crate) fn children_of(&self, node: NodeId) -> &[NodeId] {
        &self.children[node_index(node, self.elements.len())]
    }

    /// The element's parent element.
    #[inline]
    pub(crate) fn parent_of(&self, node: NodeId) -> Option<NodeId> {
        self.parent[node_index(node, self.elements.len())]
    }

    /// The node's index within its parent's ordered children vector. Valid whenever [`Self::parent_of`] is `Some`.
    #[inline]
    pub(crate) fn sibling_position(&self, node: NodeId) -> usize {
        self.sibling_pos[node_index(node, self.elements.len())] as usize
    }

    /// How many ELEMENT siblings precede `node` under its parent; none when the node has no parent element.
    #[inline]
    pub(crate) fn element_siblings_before(&self, node: NodeId) -> Option<u32> {
        self.parent_of(node)?;
        Some(self.elem_sibs_before[node_index(node, self.elements.len())])
    }

    /// The element's ELEMENT-child count.
    #[inline]
    pub(crate) fn element_children_of(&self, node: NodeId) -> u32 {
        self.elem_child_count[node_index(node, self.elements.len())]
    }

    /// The element's document-order rank.
    #[inline]
    pub(crate) fn rank_of(&self, node: NodeId) -> u32 {
        self.rank[node_index(node, self.elements.len())]
    }

    /// The string content of a TEXT leaf; empty for non-text nodes.
    #[inline]
    pub(crate) fn leaf_text(&self, node: NodeId) -> &str {
        self.leaf_text[node_index(node, self.elements.len())].as_str()
    }

    /// Whether a node is a TEXT leaf of the language's markup projection.
    #[inline]
    pub(crate) fn is_text_leaf(&self, node: NodeId) -> bool {
        self.leaf[node_index(node, self.elements.len())] == LeafSort::Text
    }

    /// Every element id in document (pre-)order.
    pub(crate) fn element_ids_in_document_order(&self) -> Result<Vec<NodeId>, SelectorError> {
        let mut ids: Vec<NodeId> = (0..self.elements.len())
            .filter(|index| self.elements[*index])
            .map(|index| {
                NodeId::try_from_index(index).ok_or_else(|| SelectorError::Internal {
                    contract: "node id overflow",
                })
            })
            .collect::<Result<_, _>>()?;
        ids.sort_by_key(|node| self.rank_of(*node));
        Ok(ids)
    }
}

/// Converts a document-local node id to a dense index with a contract check.
#[inline]
pub(crate) fn node_index(node: NodeId, node_count: usize) -> usize {
    let index = node.get() as usize;
    debug_assert!(index < node_count, "node id outside the document");
    index
}
