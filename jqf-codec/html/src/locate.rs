//! Exact-path navigation over the recovered HTML tree for the located route.
//!
//! The semantic document's value for the document element is an ARRAY of its recovered children (each child element and
//! text run is one array item; comments are ATTACHED FACTS, never items — §4.10's comment model). Navigation therefore
//! walks the tree by array-index and array-range steps only, over the SAME child projection the semantic builder uses,
//! so a located selection and the whole-document floor always agree.

use alloc::vec::Vec;

use crate::tree::{NodeId, NodeKind, Tree};
use jqf_codec_core::{CodecError, markup};
use jqf_data::ValueKind;

/// The accessor hint for a missed member step, mirroring the XML locate arm's law over the HTML tree: when the missed
/// name equals the element's own (adjusted) name or one of its attributes, the hint names the `.@name` / `.&name`
/// accessor — except on the DOCUMENT element, where this miss is the projection seam and the hint says the root element
/// IS the key (navigate children directly, or read the name with `.@name`). `None` when nothing matches, so the message
/// stays byte-identical for a name matching nothing.
fn member_step_hint(tree: &Tree, node: &Located, key: &[u8]) -> Option<alloc::string::String> {
    let element = match node {
        Located::Element(id) => *id,
        Located::Range { children } => children.iter().find_map(|child| match child {
            Child::Element(id) => Some(*id),
            Child::Leaf { .. } => None,
        })?,
        // A leaf or a prior mismatch: no element to probe, no hint.
        Located::Leaf { .. } | Located::Missing { .. } | Located::TypeMismatch { .. } => {
            return None;
        }
    };
    let element_node = &tree.nodes[element];
    if element_node.name.as_bytes() == key {
        // On the DOCUMENT element this miss is the projection seam: the shown JSON wraps the root under its own key
        // (`{"html": …}`), so a user descending from that shape writes `.html` — but the root IS the html element and
        // its own name is a fact, never a child. The actionable hint names the real navigation (children by name) and
        // the accessor that reads the name itself; on a NESTED element the accessor is the whole answer.
        if matches!(node, Located::Element(id) if Some(NodeId(*id)) == tree.document_element()) {
            return Some(markup::root_element_miss_hint(&alloc::string::String::from_utf8_lossy(
                key,
            )));
        }
        return Some(alloc::string::String::from(markup::OWN_NAME_MISS_HINT));
    }
    if element_node.attrs.iter().any(|attr| attr.name.as_bytes() == key) {
        return Some(markup::attribute_miss_hint(&alloc::string::String::from_utf8_lossy(
            key,
        )));
    }
    None
}

/// A located child of an element: an element subtree or a text leaf.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Child {
    /// A child element, by tree index.
    Element(usize),
    /// A text leaf, by its parent element's tree index and content position.
    Leaf { parent: usize, position: usize },
}

/// The value located at an exact path.
#[derive(Debug)]
pub(crate) enum Located {
    /// An element (an array of its children).
    Element(usize),
    /// A text leaf.
    Leaf { parent: usize, position: usize },
    /// A contiguous range of an element's children.
    Range { children: Vec<Child> },
    /// The path was absent at this zero-based step.
    Missing { step: usize },
    /// A step addressed a non-iterable category.
    TypeMismatch {
        /// The step index that failed.
        step: usize,
        /// The kind of the value the step was applied TO.
        actual: ValueKind,
        /// The markup accessor hint for a missed member step (the name is an attribute or the element's own name),
        /// threaded to the engine's mismatch message — what keeps the pushed-down route's message byte-identical to the
        /// engine floor's. `None` for a leaf descent or a name matching nothing.
        hint: Option<alloc::string::String>,
    },
}

/// The owned exact-path vocabulary is core's: every pushed-down route of every codec copies the requirement's
/// [`jqf_codec_core::PortableStep`]s the same way, for the same session lifetime.
pub(crate) use jqf_codec_core::{OwnedStep, own_steps};

/// The semantic children of one element: the text runs and child elements in tree order (comments and doctypes are
/// facts, never array items).
pub(crate) fn semantic_children(tree: &Tree, element: usize) -> Vec<Child> {
    let mut children = Vec::new();
    for (position, child) in tree.nodes[element].children.iter().enumerate() {
        match tree.nodes[child.0].kind {
            NodeKind::Text => children.push(Child::Leaf {
                parent: element,
                position,
            }),
            NodeKind::Element => children.push(Child::Element(child.0)),
            NodeKind::Comment | NodeKind::Doctype | NodeKind::Document => {}
        }
    }
    children
}

/// Walks the exact path from the document root over the recovered tree.
pub(crate) fn locate(tree: &Tree, steps: &[OwnedStep]) -> Located {
    let mut node: Located = tree
        .document_element()
        .map_or(Located::Missing { step: 0 }, |child| Located::Element(child.0));
    for (index, step) in steps.iter().enumerate() {
        match step {
            // A member step over a markup ELEMENT array (or a selected child sequence) navigates the children by
            // ELEMENT NAME — plural, matching every repeated element in order — instead of the array-with-string
            // mismatch. A member step over a selected sequence navigates INTO each selected element (the generator
            // composition). An unmatched name keeps the ordinary hard mismatch, never a silent null.
            OwnedStep::Member(name) => {
                let children: Vec<Child> = match &node {
                    Located::Element(element) => semantic_children(tree, *element)
                        .into_iter()
                        .filter(|child| {
                            matches!(child, Child::Element(id)
                                if tree.nodes[*id].name.as_bytes() == name.as_str().as_bytes())
                        })
                        .collect(),
                    Located::Range { children } => children
                        .iter()
                        .flat_map(|child| match child {
                            Child::Element(element) => semantic_children(tree, *element)
                                .into_iter()
                                .filter(|grandchild| {
                                    matches!(grandchild, Child::Element(id)
                                        if tree.nodes[*id].name.as_bytes() == name.as_str().as_bytes())
                                })
                                .collect::<Vec<_>>(),
                            Child::Leaf { .. } => Vec::new(),
                        })
                        .collect(),
                    Located::Leaf { .. } => {
                        return Located::TypeMismatch {
                            step: index,
                            actual: ValueKind::String,
                            hint: None,
                        };
                    }
                    Located::Missing { .. } | Located::TypeMismatch { .. } => return node,
                };
                if children.is_empty() {
                    return Located::TypeMismatch {
                        step: index,
                        actual: ValueKind::Array,
                        hint: member_step_hint(tree, &node, name.as_str().as_bytes()),
                    };
                }
                // A SINGLE match resolves to the element itself (facts preserved, `[0]` composes over its own
                // children); several matches are the ordered RANGE used while walking remaining steps. Publishing that
                // range as an array root is declined: a plural member is a stream.
                node = match children.as_slice() {
                    [Child::Element(element)] => Located::Element(*element),
                    _ => Located::Range { children },
                };
            }
            OwnedStep::Index(raw) => {
                let children = match node {
                    Located::Element(id) => semantic_children(tree, id),
                    Located::Leaf { .. } => {
                        return Located::TypeMismatch {
                            step: index,
                            actual: ValueKind::String,
                            hint: None,
                        };
                    }
                    // A range (a slice or the member-step plural) is an array in the value model: the reference indexes
                    // it legally.
                    Located::Range { children } => children.clone(),
                    Located::Missing { .. } | Located::TypeMismatch { .. } => return node,
                };
                // Same wrap-from-the-end law as [`jqf_data::resolve_index`] — called directly, so an `i64::MIN` index
                // cannot overflow the arithmetic; out of range is `Missing`.
                let Some(position) = jqf_data::resolve_index(children.len(), *raw) else {
                    return Located::Missing { step: index };
                };
                node = match children[position] {
                    Child::Element(id) => Located::Element(id),
                    Child::Leaf { parent, position } => Located::Leaf { parent, position },
                };
            }
            OwnedStep::Range { start, end } => {
                let children = match node {
                    Located::Element(id) => semantic_children(tree, id),
                    Located::Leaf { .. } => {
                        return Located::TypeMismatch {
                            step: index,
                            actual: ValueKind::String,
                            hint: None,
                        };
                    }
                    // A range (a slice or the member-step plural) is an array in the value model, so the reference
                    // SLICES it — the same law the index arm above states, and what the whole-document floor does for
                    // `.body.p[0:2]`.
                    Located::Range { children } => children,
                    Located::Missing { .. } | Located::TypeMismatch { .. } => return node,
                };
                let len = children.len() as i64;
                // A strictly negative bound counts from the end; saturating arithmetic keeps an `i64::MIN` bound
                // clamped at the start instead of relying on wrapping.
                let begin = match start {
                    Some(raw) if *raw < 0 => len.saturating_add(*raw).max(0),
                    Some(raw) => (*raw).min(len).max(0),
                    None => 0,
                };
                let finish = match end {
                    Some(raw) if *raw < 0 => len.saturating_add(*raw).max(0),
                    Some(raw) => (*raw).min(len).max(0),
                    None => len,
                };
                if begin >= finish {
                    return Located::Range { children: Vec::new() };
                }
                node = Located::Range {
                    children: children[begin as usize..finish as usize].to_vec(),
                };
            }
        }
    }
    node
}

pub(crate) fn data_contract(what: &'static str) -> CodecError {
    jqf_codec_core::data_contract(what)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_codec_core::PortableStep;

    pub(crate) fn member(name: &str) -> PortableStep {
        PortableStep::SemanticMember(name.to_owned())
    }

    #[test]
    fn a_range_over_a_plural_member_slices_it() {
        // `.body.p[0:2]`: three `p` children make the member step plural (a `Range`), and a range over an array is
        // legal everywhere else — the whole-document floor returns the slice, so the pushed-down route must too, never
        // a cannot-index mismatch.
        let tree = crate::tree::TreeBuilder::build("<body><p>a</p><p>b</p><p>c</p></body>");
        let steps = own_steps(&[
            member("body"),
            member("p"),
            PortableStep::SemanticRange {
                start: Some(0),
                end: Some(2),
            },
        ])
        .expect("owned steps");
        match locate(&tree, steps.as_slice()) {
            Located::Range { children } => assert_eq!(children.len(), 2),
            other => panic!("a range over a plural member must slice it: {other:?}"),
        }
    }

    #[test]
    fn a_single_child_member_locates_the_element() {
        let tree = crate::tree::TreeBuilder::build("<body><p>hi</p></body>");
        let steps = own_steps(&[member("body"), member("p")]).expect("owned steps");
        match locate(&tree, steps.as_slice()) {
            Located::Element(id) => assert_eq!(tree.nodes[id].name, "p"),
            other => panic!("single member must locate the element: {other:?}"),
        }
    }

    /// Two `p` children: the tree locator returns a range. The located session declines that spelling so the floor can
    /// stream — see `tests/scoped.rs` `a_plural_member_declines_located_so_the_floor_can_stream`.
    #[test]
    fn a_plural_child_member_locates_a_range() {
        let tree = crate::tree::TreeBuilder::build("<body><p>hi</p><p>bye</p></body>");
        let steps = own_steps(&[member("body"), member("p")]).expect("owned steps");
        match locate(&tree, steps.as_slice()) {
            Located::Range { children } => assert_eq!(children.len(), 2),
            other => panic!("plural member must locate a range: {other:?}"),
        }
    }

    #[test]
    fn an_i64_min_index_is_missing_not_an_overflow() {
        // The wrap-from-the-end law resolves by subtracting the magnitude; i64::MIN is out of range for any real
        // length, so the answer is Missing — never an addition that panics a debug build.
        let tree = crate::tree::TreeBuilder::build("<body><p>a</p><p>b</p></body>");
        let steps =
            own_steps(&[member("body"), member("p"), PortableStep::SemanticIndex(i64::MIN)]).expect("owned steps");
        assert!(matches!(locate(&tree, steps.as_slice()), Located::Missing { .. }));
    }

    #[test]
    fn an_i64_min_range_bound_clamps_instead_of_overflowing() {
        // A strictly negative bound counts from the end and clamps at the start; i64::MIN saturates there rather than
        // wrapping.
        let tree = crate::tree::TreeBuilder::build("<body><p>a</p><p>b</p></body>");
        let steps = own_steps(&[
            member("body"),
            member("p"),
            PortableStep::SemanticRange {
                start: Some(i64::MIN),
                end: None,
            },
        ])
        .expect("owned steps");
        match locate(&tree, steps.as_slice()) {
            Located::Range { children } => assert_eq!(children.len(), 2),
            other => panic!("an i64::MIN start must clamp to the full slice: {other:?}"),
        }
    }
}
