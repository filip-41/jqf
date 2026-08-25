//! The shared comment-position vocabulary and the ownership-precedence rule.
//!
//! A document comment occupies exactly one of three positions relative to the node it attaches to: LEADING (a block
//! above the node), INLINE (on the same logical line as the node's own value), or FOOT (below the node's closing
//! block). Each codec names its comment facts from this vocabulary — `<format>.comment@1`,
//! `<format>.comment_inline@1`, `<format>.comment_foot@1` — because a document carries exactly one codec's facts, so
//! the namespaces cannot collide and the semantic segment is what the `.@name` selector engine matches on
//! ([`crate::markup`] is the sibling vocabulary for the markup formats).
//!
//! **The ownership-precedence rule.** When a comment run sits between two candidate owners, it belongs to exactly one
//! of them, decided in this order:
//!
//! 1. A comment on the SAME LOGICAL LINE as the end of the previous occurrence (nothing but whitespace between the
//!    previous node's span end and the comment) is that previous occurrence's INLINE comment: the    line carries it,
//!    so the line owns it.
//! 2. Otherwise, when the grammar admits the comment as a LEADING candidate for the next owner (a flush-left block
//!    above a following node), the next owner takes it as its leading comment.
//! 3. Otherwise — the comment sits below a closing block that does not admit a following owner, or no next node
//!    follows — the PREVIOUS node (or its container) owns it as its FOOT. The document trailer is always the
//!    ROOT's foot.
//!
//! The rule is codec-agnostic by construction: an ownership precedence that differs per format is the defect this
//! vocabulary exists to prevent. A format's own column/token rule for FINDING a foot stays in its codec.

/// The leading-comment position's semantic segment: the `<format>.comment@1` role. Its selector is `.@comment`, with
/// `.@comment_head` ([`HEAD_ALIAS`]) as a second spelling.
pub const HEAD: &str = "comment";

/// The same-line-comment position's semantic segment: the `<format>.comment_inline@1` role. Its selector is
/// `.@comment_inline`.
pub const INLINE: &str = "comment_inline";

/// The foot-comment position's semantic segment: the `<format>.comment_foot@1` role. Its selector is `.@comment_foot`.
pub const FOOT: &str = "comment_foot";

/// The `.@comment_head` selector alias of [`HEAD`]: a second, permanent spelling of the canonical `comment` selector,
/// normalized to `comment` AT LOWERING so nothing downstream sees the alias — the read comparator and the write
/// allow-list both receive the canonical role, and `json_facts` projects exactly one `@comment` key, never
/// `@comment_head`.
pub const HEAD_ALIAS: &str = "comment_head";

/// Builds the leading-comment fact role for one codec: `<format>.comment@1` (the codec's own `COMMENT_FACT` constants
/// are this spelling, kept `'static` for the schema recipe).
#[must_use]
pub fn comment_role(format: &str) -> alloc::string::String {
    alloc::format!("{format}.{HEAD}@1")
}

/// One query-time comment write the encoder overlays onto a document's comment index. `lines` of `None` (or an empty
/// list) deletes the fact.
#[derive(Clone, Debug)]
pub struct CommentEncodeOverlay {
    /// Owning node in the original document.
    pub node: jqf_data::NodeId,
    /// Semantic segment: [`HEAD`], [`INLINE`], or [`FOOT`].
    pub role: alloc::string::String,
    /// Replacement lines, or `None` to delete.
    pub lines: Option<alloc::vec::Vec<alloc::string::String>>,
}

/// Last-write-wins overlay of [`CommentEncodeOverlay`] entries stashed on
/// [`jqf_resource::ResourceContext::host_extension`] onto a comment index.
///
/// Query-time fact writes (`PATH.@comment = …` without `--edit`) record deltas the document itself does not carry;
/// YAML/TOML encode reads this overlay so a re-encode emits the written comments. `--edit` splices retained source and
/// does not install the overlay.
pub fn apply_encode_overlay(
    leading: &mut alloc::collections::BTreeMap<jqf_data::NodeId, alloc::vec::Vec<alloc::string::String>>,
    inline: &mut alloc::collections::BTreeMap<jqf_data::NodeId, alloc::vec::Vec<alloc::string::String>>,
    foot: &mut alloc::collections::BTreeMap<jqf_data::NodeId, alloc::vec::Vec<alloc::string::String>>,
    resources: &jqf_resource::ResourceContext<'_>,
) {
    let Some(overlay) = resources
        .host_extension()
        .and_then(|ext| ext.downcast_ref::<alloc::vec::Vec<CommentEncodeOverlay>>())
    else {
        return;
    };
    for item in overlay {
        let map = match item.role.as_str() {
            HEAD => &mut *leading,
            INLINE => &mut *inline,
            FOOT => &mut *foot,
            // Unknown roles are DELIBERATELY IGNORED, never rejected: overlay roles arrive from outside this
            // vocabulary, and an unknown one must not fail the run — it simply has no comment index to land in.
            _ => continue,
        };
        match &item.lines {
            Some(lines) if !lines.is_empty() => {
                map.insert(item.node, lines.clone());
            }
            _ => {
                map.remove(&item.node);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builders_spell_the_three_roles() {
        assert_eq!(comment_role("yaml"), "yaml.comment@1");
        assert_eq!(alloc::format!("yaml.{INLINE}@1"), "yaml.comment_inline@1");
        assert_eq!(alloc::format!("yaml.{FOOT}@1"), "yaml.comment_foot@1");
        assert_eq!(comment_role("toml"), "toml.comment@1");
        assert_eq!(alloc::format!("toml.{INLINE}@1"), "toml.comment_inline@1");
        assert_eq!(alloc::format!("toml.{FOOT}@1"), "toml.comment_foot@1");
    }
}
