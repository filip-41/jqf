//! The HTML element-directionality half of `html.css@1` `:dir()`.
//!
//! `:dir(ltr)` / `:dir(rtl)` follow the pinned HTML element-directionality algorithm over recovered attributes and text
//! only (no stylesheet, no computed style, no browser default). The algorithm consults the Unicode bidirectional
//! character classes L, AL, and R; the range tables live in [`crate::selector::bidi_ranges`], generated from the pinned
//! Unicode 17.0.0 UCD.zip (the design's research manifest digest verifies). The tables are searched by binary probe;
//! the ranges are disjoint and sorted, so a probe is exact.
//!
//! The element-directionality algorithm as pinned:
//!
//! 1. `dir="ltr"` → `ltr`; `dir="rtl"` → `rtl`; `dir="auto"` → the auto directionality of the element (the first
//!    strong L/AL/R character of its descendant text, skipping `bdi`/`script`/`style`/`textarea` subtrees and
//!    elements with their own `dir`), `ltr` when none.
//! 2. No `dir` attribute → the parent element's directionality; the document element defaults to `ltr`.
//!
//! Form-associated auto (`input`/`textarea` values) is out of scope for the static tree: `input` has no value
//! projection, and `textarea`'s text IS its descendant text, which the general rule already scans.

use super::index::MarkupIndex;

/// The result of the directionality computation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Direction {
    Ltr,
    Rtl,
}

/// Whether one code point is of bidirectional character type L, AL, or R.
fn strong_class(code: u32) -> Option<Direction> {
    if in_ranges(code, crate::selector::bidi_ranges::L) {
        return Some(Direction::Ltr);
    }
    if in_ranges(code, crate::selector::bidi_ranges::AL) || in_ranges(code, crate::selector::bidi_ranges::R) {
        return Some(Direction::Rtl);
    }
    None
}

/// Binary probe over one disjoint sorted range table.
fn in_ranges(code: u32, table: &[(u32, u32)]) -> bool {
    table
        .binary_search_by(|(start, end)| {
            if code < *start {
                core::cmp::Ordering::Greater
            } else if code > *end {
                core::cmp::Ordering::Less
            } else {
                core::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// The directionality of one element per the pinned algorithm.
pub(crate) fn directionality(index: &MarkupIndex, node: jqf_data::NodeId) -> Direction {
    let mut current = node;
    loop {
        match index.attr(current, "dir") {
            Some(value) if value.eq_ignore_ascii_case("ltr") => return Direction::Ltr,
            Some(value) if value.eq_ignore_ascii_case("rtl") => return Direction::Rtl,
            Some(value) if value.eq_ignore_ascii_case("auto") => {
                return auto_directionality(index, current).unwrap_or(Direction::Ltr);
            }
            _ => {}
        }
        match index.parent_of(current) {
            Some(parent) => current = parent,
            None => return Direction::Ltr,
        }
    }
}

/// The auto directionality: the first strong character of the element's descendant text in tree order, scanning only
/// text nodes that are not inside a `bdi`/`script`/`style`/`textarea` element and not inside an element with its own
/// `dir` attribute.
fn auto_directionality(index: &MarkupIndex, node: jqf_data::NodeId) -> Option<Direction> {
    // Iterative pre-order walk of the element subtree. A text leaf is scanned when no ancestor in the walk is excluded;
    // exclusion is tracked by the walk itself (an excluded subtree is simply not descended).
    let mut stack: alloc::vec::Vec<(jqf_data::NodeId, bool)> = alloc::vec![(node, false)];
    while let Some((cursor, excluded)) = stack.pop() {
        if excluded {
            continue;
        }
        if index.is_text_leaf(cursor) {
            if let Some(direction) = first_strong(index.leaf_text(cursor)) {
                return Some(direction);
            }
            continue;
        }
        // An element: exclude the subtree when it is one of the pinned containers or carries its own dir attribute. The
        // walk ROOT's own dir attribute is the `auto` being resolved and must not exclude its own text; only DESCENDANT
        // elements' dir attributes exclude (WHATWG element-directionality).
        let name = index.name_of(cursor);
        let own_dir = cursor != node && index.attr(cursor, "dir").is_some();
        let skip = own_dir || matches!(name, "bdi" | "script" | "style" | "textarea");
        let child_excluded = excluded || skip;
        for child in index.children_of(cursor).iter().rev() {
            stack.push((*child, child_excluded));
        }
    }
    None
}

/// The first strong L/AL/R character of a string's first text content.
fn first_strong(text: &str) -> Option<Direction> {
    for ch in text.chars() {
        if let Some(direction) = strong_class(ch as u32) {
            return Some(direction);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_are_disjoint_sorted_and_cover_the_obvious_ranges() {
        for table in [
            crate::selector::bidi_ranges::L,
            crate::selector::bidi_ranges::AL,
            crate::selector::bidi_ranges::R,
        ] {
            let mut previous_end = None;
            for (start, end) in table {
                assert!(start <= end);
                if let Some(prior) = previous_end {
                    assert!(*start > prior, "overlapping ranges");
                }
                previous_end = Some(*end);
            }
        }
        assert_eq!(strong_class('A' as u32), Some(Direction::Ltr));
        assert_eq!(strong_class('א' as u32), Some(Direction::Rtl));
        assert_eq!(strong_class(' ' as u32), None);
        assert_eq!(strong_class(0x200F as u32), Some(Direction::Rtl)); // RLM
    }

    #[test]
    fn first_strong_finds_the_first_strong_character() {
        assert_eq!(first_strong("  hello"), Some(Direction::Ltr));
        assert_eq!(first_strong("123 \u{05D0}"), Some(Direction::Rtl));
        assert_eq!(first_strong(" \t\n"), None);
    }
}
