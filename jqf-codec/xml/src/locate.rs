//! Exact-path location for the XML scoped route (slot 1, `Exact`/
//! `Located`), applied DURING the validate-everything parse: the parser
//! records a stack of child ledgers along the remaining path and
//! [`apply_steps`] walks those ledgers, so the scoped session never
//! materializes the whole tree and never re-parses a nested extent.
//!
//! The whole XML document's value is the root ELEMENT: an array of its
//! ordered children (each child element, text run, comment, or processing
//! instruction is one array item). Navigation therefore walks the children by
//! array-index, array-range and MEMBER steps — the format-neutral
//! [`PortableStep`] vocabulary over an array-shaped value. A `SemanticMember`
//! step navigates the element's children by CLARK name, PLURAL (every
//! repeated element in order, as a range), the same text `.@name` serves; an
//! unmatched name is the ordinary non-iterable category mismatch the floor
//! produces, carrying the accessor hint.

use alloc::string::String;
use alloc::vec::Vec;

use crate::value::{ExpandedName, NameInterner};
use jqf_codec_core::markup;
use jqf_data::{ValueKind, resolve_index};

/// A locate-during-parse hit: byte extents or already-decoded leaves, so the
/// scoped route never materializes the whole Tree.
#[derive(Debug)]
pub(crate) enum LocatedHit {
    /// A complete element's source extent `[start, end)` and the direct-child
    /// count recorded while that element was open on the proving walk.
    Element {
        start: usize,
        end: usize,
        child_count: u64,
    },
    /// A decoded scalar leaf.
    Leaf {
        kind: &'static str,
        value: String,
    },
    /// Several hits in document order. The session declines this as a stream.
    Range,
    Missing {
        step: usize,
    },
    TypeMismatch {
        step: usize,
        actual: ValueKind,
        hint: Option<String>,
    },
}

/// One direct child of a live locate frame.
#[derive(Debug)]
pub(crate) enum LocateChild {
    Element {
        name: ExpandedName,
        /// Attribute expanded names, recorded when further path steps remain
        /// so a nested miss can carry the same own-name / attribute hint.
        attrs: alloc::vec::Vec<ExpandedName>,
        start: usize,
        end: usize,
        child_count: u64,
        /// Direct children recorded on the proving walk when more path steps
        /// remain after this element. Empty at the last path depth: that
        /// level only increments `child_count`.
        children: alloc::vec::Vec<LocateChild>,
    },
    Text(String),
    Comment(String),
    ProcessingInstruction {
        target: String,
        data: String,
    },
}

impl LocateChild {
    fn as_hit(&self) -> LocatedHit {
        match self {
            LocateChild::Element {
                start,
                end,
                child_count,
                ..
            } => LocatedHit::Element {
                start: *start,
                end: *end,
                child_count: *child_count,
            },
            LocateChild::Text(text) => LocatedHit::Leaf {
                kind: crate::document::TEXT_KIND,
                value: text.clone(),
            },
            LocateChild::Comment(text) => LocatedHit::Leaf {
                kind: crate::document::COMMENT_KIND,
                value: text.clone(),
            },
            LocateChild::ProcessingInstruction { target, data } => LocatedHit::Leaf {
                kind: crate::document::PI_KIND,
                value: crate::value::pi_spelling(target, data),
            },
        }
    }
}

/// Applies remaining path steps to an element's direct children.
pub(crate) fn apply_steps(
    intern: &NameInterner,
    root_name: ExpandedName,
    root_attrs: &[ExpandedName],
    is_document_root: bool,
    children: &[LocateChild],
    steps: &[OwnedStep],
    step_offset: usize,
) -> LocatedHit {
    if steps.is_empty() {
        return LocatedHit::Range;
    }
    let step = &steps[0];
    match step {
        OwnedStep::Member(name) => {
            let key = name.as_str().as_bytes();
            let hits: Vec<&LocateChild> = children
                .iter()
                .filter(|child| match child {
                    LocateChild::Element { name: child_name, .. } => child_name.clark_eq(intern, key),
                    _ => false,
                })
                .collect();
            if hits.is_empty() {
                return LocatedHit::TypeMismatch {
                    step: step_offset,
                    actual: ValueKind::Array,
                    hint: member_hint(intern, root_name, root_attrs, is_document_root, key),
                };
            }
            match hits.as_slice() {
                [child] => apply_into_element(intern, child, &steps[1..], step_offset + 1),
                _ => LocatedHit::Range,
            }
        }
        OwnedStep::Index(signed) => {
            let Some(child) = index_locate_child(children, *signed) else {
                return LocatedHit::Missing { step: step_offset };
            };
            apply_into_element(intern, child, &steps[1..], step_offset + 1)
        }
        OwnedStep::Range { .. } => LocatedHit::Range,
    }
}

/// Applies any remaining steps to a unique child already recorded on the
/// proving walk. An empty remainder is the child's own hit.
fn apply_into_element(
    intern: &NameInterner,
    child: &LocateChild,
    rest: &[OwnedStep],
    next_offset: usize,
) -> LocatedHit {
    if rest.is_empty() {
        return child.as_hit();
    }
    match child {
        LocateChild::Element {
            name, attrs, children, ..
        } => apply_steps(intern, *name, attrs, false, children, rest, next_offset),
        _ => LocatedHit::TypeMismatch {
            step: next_offset,
            actual: ValueKind::String,
            hint: None,
        },
    }
}

fn index_locate_child(children: &[LocateChild], signed: i64) -> Option<&LocateChild> {
    children.get(resolve_index(children.len(), signed)?)
}

fn member_hint(
    intern: &NameInterner,
    root_name: ExpandedName,
    root_attrs: &[ExpandedName],
    is_document_root: bool,
    key: &[u8],
) -> Option<String> {
    if root_name.clark_eq(intern, key) {
        if is_document_root {
            return Some(markup::root_element_miss_hint(&String::from_utf8_lossy(key)));
        }
        return Some(String::from(markup::OWN_NAME_MISS_HINT));
    }
    for attr in root_attrs {
        if attr.clark_eq(intern, key) {
            return Some(markup::attribute_miss_hint(&String::from_utf8_lossy(key)));
        }
    }
    None
}

/// The owned exact-path vocabulary is core's: every pushed-down route of every
/// codec copies the requirement's [`jqf_codec_core::PortableStep`]s the same
/// way, for the same session lifetime.
pub(crate) use jqf_codec_core::{OwnedStep, own_steps};

#[cfg(test)]
pub(crate) fn copy_steps(steps: &[OwnedStep]) -> Vec<OwnedStep> {
    steps
        .iter()
        .map(|step| match step {
            OwnedStep::Member(name) => OwnedStep::Member(name.clone()),
            OwnedStep::Index(index) => OwnedStep::Index(*index),
            OwnedStep::Range { start, end } => OwnedStep::Range {
                start: *start,
                end: *end,
            },
        })
        .collect()
}

/// Resolves signed range bounds against a length, following the same
/// len-relative law as an index: a strictly negative bound counts from the
/// end, then both clamp into `0..=len`.
#[cfg(test)]
fn resolve_range(len: usize, start: Option<i64>, end: Option<i64>) -> (usize, usize) {
    let len_i = len as i64;
    let resolve = |bound: Option<i64>, default: i64| -> i64 {
        match bound {
            None => default,
            // Saturating arithmetic keeps an `i64::MIN` bound clamped at the
            // start instead of relying on wrapping.
            Some(value) if value < 0 => len_i.saturating_add(value).max(0),
            Some(value) => value,
        }
    };
    let from = resolve(start, 0).min(len_i).max(0);
    let to = resolve(end, len_i).min(len_i).max(from);
    (from as usize, to as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{ParseOutput, XmlParseState};
    use jqf_codec_core::PortableStep;

    fn member(name: &str) -> PortableStep {
        PortableStep::SemanticMember(name.to_owned())
    }

    #[test]
    fn locate_during_parse_locates_the_nested_element() {
        let input = "<doc><nested><deep>x</deep></nested></doc>";
        let steps = own_steps(&[member("nested"), member("deep")]).expect("owned");
        let mut parse = XmlParseState::try_new_locate(input.as_bytes(), copy_steps(&steps)).expect("state");
        let mut resources = jqf_resource::ResourceContext::new(
            jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u32::MAX,
            ))
            .expect("account"),
            &jqf_resource::ContinueControl,
            jqf_resource::WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("resources");
        let hit = loop {
            match parse.poll(input.as_bytes(), &mut resources).expect("poll") {
                crate::parse::ParsePoll::Pending => {
                    resources.try_begin_next_cooperative_entry(1).expect("work");
                }
                crate::parse::ParsePoll::Ready(ParseOutput::Located(hit)) => break hit,
                crate::parse::ParsePoll::Ready(_) => panic!("expected locate"),
            }
        };
        match hit {
            LocatedHit::Element {
                start,
                end,
                child_count,
            } => {
                assert_eq!(
                    &input[start..end],
                    "<deep>x</deep>",
                    "nested locate must be the innermost extent, got {}",
                    &input[start..end]
                );
                assert_eq!(child_count, 1, "deep's one text child is counted on the proving walk");
            }
            other => panic!("expected an element hit, got {other:?}"),
        }
    }

    #[test]
    fn locate_during_parse_walks_a_three_step_member_path() {
        let input = "<doc><a><b><c>z</c></b></a></doc>";
        let steps = own_steps(&[member("a"), member("b"), member("c")]).expect("owned");
        let mut parse = XmlParseState::try_new_locate(input.as_bytes(), copy_steps(&steps)).expect("state");
        let mut resources = jqf_resource::ResourceContext::new(
            jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u32::MAX,
            ))
            .expect("account"),
            &jqf_resource::ContinueControl,
            jqf_resource::WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("resources");
        let hit = loop {
            match parse.poll(input.as_bytes(), &mut resources).expect("poll") {
                crate::parse::ParsePoll::Pending => {
                    resources.try_begin_next_cooperative_entry(1).expect("work");
                }
                crate::parse::ParsePoll::Ready(ParseOutput::Located(hit)) => break hit,
                crate::parse::ParsePoll::Ready(_) => panic!("expected locate"),
            }
        };
        match hit {
            LocatedHit::Element {
                start,
                end,
                child_count,
            } => {
                assert_eq!(&input[start..end], "<c>z</c>");
                assert_eq!(child_count, 1);
            }
            other => panic!("expected an element hit, got {other:?}"),
        }
    }

    #[test]
    fn an_i64_min_range_bound_clamps_instead_of_overflowing() {
        // A strictly negative bound counts from the end and clamps at the
        // start; i64::MIN saturates there rather than wrapping the addition.
        assert_eq!(resolve_range(3, Some(i64::MIN), None), (0, 3));
        assert_eq!(resolve_range(3, None, Some(i64::MIN)), (0, 0));
        // Ordinary negative bounds still count from the end.
        assert_eq!(resolve_range(3, Some(-2), None), (1, 3));
        assert_eq!(resolve_range(3, None, Some(-1)), (0, 2));
    }
}
