//! `yaml.key-equivalence@1`: the frozen mapping-key equivalence law.
//!
//! §4.8: node kind and exact resolved tag must match. Scalars then compare their tag-defined semantic value: strings by
//! Unicode scalar sequence, integers by mathematical value, booleans/null by value, and floats numerically with `-0.0
//! == +0.0` and every fixed YAML NaN equal to every other YAML NaN. Thus an integer `1` and float `1.0` remain distinct
//! because their tags differ. Anchors and presentation are ignored; aliases compare their resolved targets. Sequences
//! compare in order. Mappings compare as unordered sets of recursively equivalent key/value pairs, not source order.
//!
//! Cycles use a coinductive in-progress stack: a revisited in-progress pair is provisionally equal, and any
//! kind/tag/scalar/edge mismatch invalidates it. Resource exhaustion is an error, never "different".
//!
//! The comparator works over the codec's own graph arena (scalar text and resolved tags) BEFORE object projection, so
//! it needs no `Document`. It is iterative in the sense that the recursion depth is bounded by the graph's node count
//! and every collection comparison walks the arenas — YAML key graphs are never pathologically deep beyond the input's
//! own nesting, which the scanner already bounds iteratively.

use alloc::borrow::Cow;
use alloc::vec;
use alloc::vec::Vec;

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::graph::{NodeId, YamlGraph, YamlNode};
use crate::provider::DialectKind;
use crate::schema::{
    self, TAG_BOOL, TAG_FLOAT, TAG_INT, TAG_MAP, TAG_NULL, TAG_SEQ, TAG_STR, is_infinity_spelling, is_nan_spelling,
    is_negative_infinity, is_standard_tag,
};

/// The equality verdict.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Verdict {
    Equal,
    Different,
}

/// The recursive coinductive comparator. One instance per comparison batch; the in-progress stack is reset between
/// top-level calls.
pub(crate) struct KeyEquality<'graph> {
    graph: &'graph YamlGraph,
    source: ResolvedSource<'graph>,
    /// The schema scalars resolve under: the law compares the RESOLVED tag, so a quoted `"a"` and an explicit `!!str a`
    /// are the same key.
    dialect: DialectKind,
    /// In-progress pairs on the current recursion path (coinduction).
    active: Vec<(NodeId, NodeId)>,
}

impl<'graph> KeyEquality<'graph> {
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn try_new(
        graph: &'graph YamlGraph,
        source: ResolvedSource<'graph>,
        dialect: DialectKind,
    ) -> Result<Self, CodecError> {
        Ok(Self {
            graph,
            source,
            dialect,
            active: Vec::new(),
        })
    }

    /// Compares two graph nodes under the law.
    #[allow(clippy::unnecessary_wraps, clippy::unused_self)]
    pub(crate) fn equals(
        &mut self,
        left: NodeId,
        right: NodeId,
        resources: &ResourceContext<'_>,
    ) -> Result<Verdict, CodecError> {
        self.active.clear();
        self.node_equal(left, right, resources)
    }

    fn node_equal(
        &mut self,
        left: NodeId,
        right: NodeId,
        resources: &ResourceContext<'_>,
    ) -> Result<Verdict, CodecError> {
        // The nesting guard: the comparator recurses once per container level (the 10000-level ceiling — the
        // stack-depth gate's YAML lane).
        let _depth = resources.enter_nesting().map_err(CodecError::from)?;
        let left = self.graph.follow_alias(left, self.source)?;
        let right = self.graph.follow_alias(right, self.source)?;
        if left == right {
            return Ok(Verdict::Equal);
        }
        // Coinduction: a revisited in-progress pair is provisionally equal.
        if self.active.iter().any(|(a, b)| *a == left && *b == right) {
            return Ok(Verdict::Equal);
        }
        let (Some(left_node), Some(right_node)) = (
            self.graph.node_opt(left, self.source),
            self.graph.node_opt(right, self.source),
        ) else {
            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "YAML key comparison over a missing node",
            }));
        };
        // Kind + exact RESOLVED tag must match. The explicit tag is not the identity: a quoted `"a"` stores no tag and
        // `!!str a` stores `tag:yaml.org,2002:str`, yet both resolve to the string category — one key under the law, so
        // `{!!str a: 1, a: 2}` is a duplicate.
        if !same_kind(&left_node, &right_node) {
            return Ok(Verdict::Different);
        }
        let left_tag = self.resolved_tag(left)?;
        let right_tag = self.resolved_tag(right)?;
        if left_tag != right_tag {
            return Ok(Verdict::Different);
        }
        self.active.push((left, right));
        let verdict = match (left_node, right_node) {
            (YamlNode::Scalar { text: l, .. }, YamlNode::Scalar { text: r, .. }) => {
                scalar_equivalent(l, r, Some(left_tag.as_ref()))
            }
            (YamlNode::Sequence { items: l, .. }, YamlNode::Sequence { items: r, .. }) => {
                if l.len() == r.len() {
                    let mut verdict = Verdict::Equal;
                    for (a, b) in l.iter().zip(r) {
                        if self.node_equal(*a, *b, resources)? == Verdict::Different {
                            verdict = Verdict::Different;
                            break;
                        }
                    }
                    verdict
                } else {
                    Verdict::Different
                }
            }
            (YamlNode::Mapping { entries: l, .. }, YamlNode::Mapping { entries: r, .. }) => {
                if l.len() == r.len() {
                    // Unordered equivalence: find a distinct right entry for every left entry via a perfect matching.
                    let left_entries: Vec<(NodeId, NodeId)> = l.to_vec();
                    let right_entries: Vec<(NodeId, NodeId)> = r.to_vec();
                    let mut used = vec![false; right_entries.len()];
                    self.match_entries(&left_entries, &right_entries, &mut used, resources)?
                } else {
                    Verdict::Different
                }
            }
            _ => Verdict::Different,
        };
        self.active.pop();
        Ok(verdict)
    }

    /// Matching of left entries to distinct right entries.
    ///
    /// The pair condition (key AND value both equivalent) is precomputed into an L×R adjacency matrix, then a perfect
    /// matching is found with Kuhn's augmenting-path algorithm. The previous backtracker re-scanned from right index 0
    /// after every pop and re-created the same assignment, spinning forever on complex keys with no perfect matching.
    /// The matrix is O(L²·R) `node_equal` calls worst case — small mappings, fine.
    fn match_entries(
        &mut self,
        left: &[(NodeId, NodeId)],
        right: &[(NodeId, NodeId)],
        used: &mut [bool],
        resources: &ResourceContext<'_>,
    ) -> Result<Verdict, CodecError> {
        // adjacency[l][r] = the pair condition holds for (left[l], right[r]).
        let mut adjacency = vec![vec![false; right.len()]; left.len()];
        for (l, (lk, lv)) in left.iter().enumerate() {
            for (r, (rk, rv)) in right.iter().enumerate() {
                let key_ok = self.node_equal(*lk, *rk, resources)? == Verdict::Equal;
                let value_ok = key_ok && self.node_equal(*lv, *rv, resources)? == Verdict::Equal;
                adjacency[l][r] = key_ok && value_ok;
            }
        }
        // Kuhn's augmenting-path perfect matching: `matched` maps each right entry to its left partner; `used` is the
        // per-iteration visited set.
        let mut matched: Vec<Option<usize>> = vec![None; right.len()];
        for l in 0..left.len() {
            used.fill(false);
            if !try_augment(l, &adjacency, used, &mut matched) {
                return Ok(Verdict::Different);
            }
        }
        Ok(Verdict::Equal)
    }
}

/// One DFS step of Kuhn's algorithm: find an augmenting path from left node `left`, reassigning matched right nodes
/// along the way. `visited` is reset per augment attempt; `matched[r]` names the left node currently matched to right
/// node `r`. Recursion depth is bounded by the entry count, so at most the mapping's own size.
fn try_augment(left: usize, adjacency: &[Vec<bool>], visited: &mut [bool], matched: &mut [Option<usize>]) -> bool {
    for (r, &edge) in adjacency[left].iter().enumerate() {
        if !edge || visited[r] {
            continue;
        }
        visited[r] = true;
        if let Some(previous) = matched[r] {
            if try_augment(previous, adjacency, visited, matched) {
                matched[r] = Some(left);
                return true;
            }
        } else {
            matched[r] = Some(left);
            return true;
        }
    }
    false
}

/// Whether two nodes share a kind (aliases were already followed).
fn same_kind<'a>(left: &YamlNode<'a>, right: &YamlNode<'a>) -> bool {
    matches!(
        (left, right),
        (YamlNode::Scalar { .. }, YamlNode::Scalar { .. })
            | (YamlNode::Sequence { .. }, YamlNode::Sequence { .. })
            | (YamlNode::Mapping { .. }, YamlNode::Mapping { .. })
    )
}

/// A collection's comparison tag: its exact explicit spelling, or the schema-default collection tag when untagged (so
/// an explicit `!!seq` and a bare sequence are one kind of node).
fn collection_tag<'a>(explicit: Option<&'a str>, default: &'static str) -> Cow<'a, str> {
    match explicit {
        Some(tag) => Cow::Borrowed(tag),
        None => Cow::Borrowed(default),
    }
}

/// A merge-expansion candidate key must be a SCALAR.
///
/// This is the one production site that can hand [`KeyEquality`] a pair over non-scalar nodes: the whole-document build
/// gates every key through its object-key check and the duplicate-key walk through its core-string filter BEFORE
/// comparing, but merged mappings' keys meet host keys in [`crate::parse`]'s expansion before any such check exists. A
/// candidate whose key resolves to a sequence or mapping (aliases are followed first) refuses here under the merge-key
/// diagnostic instead of entering the comparator's container arms. Every input refused here was already unrepresentable
/// downstream — a complex key never becomes an object key — so the refusal moves the diagnostic earlier without moving
/// the accept set.
pub(crate) fn require_scalar_merge_key(
    graph: &YamlGraph,
    key: NodeId,
    source: ResolvedSource<'_>,
) -> Result<(), CodecError> {
    // Follow aliases: an aliased key is the key its target resolves to.
    let mut id = key;
    while let Some(YamlNode::Alias(target)) = graph.node_opt(id, source) {
        id = target;
    }
    match graph.node_opt(id, source) {
        Some(YamlNode::Scalar { .. }) => Ok(()),
        Some(_) => {
            let span = graph.node_span(key);
            Err(crate::error::invalid_range(
                source,
                span.start() as usize,
                span.end() as usize,
                "merge-key",
                "a merged ('<<') mapping key must be a scalar; sequence and mapping keys cannot become object keys",
            ))
        }
        None => Err(CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "YAML merge candidate key over a missing node",
        })),
    }
}

impl<'graph> KeyEquality<'graph> {
    /// The comparison identity of one node's tag: the tag the schema RESOLVES it to, not the explicit spelling. Scalars
    /// resolve through [`crate::schema::resolve_scalar`] (a plain `1` is an int, `"1"` and `!!str 1` are strings, a
    /// non-core `!money` stays itself); an untagged collection resolves to its schema-default collection tag. One alias
    /// level is followed so an alias compares as its target. A resolution error propagates: the identical input fails
    /// scalar resolution at build time with the same diagnostic.
    fn resolved_tag(&self, id: NodeId) -> Result<Cow<'graph, str>, CodecError> {
        // Copy the graph/shared handles out first: the returned identity borrows the GRAPH (tag texts are interned or
        // 'static), never `&self`, so callers may hold it across their own mutations.
        let graph: &'graph YamlGraph = self.graph;
        let source = self.source;
        let dialect = self.dialect;
        let id = match graph.node_opt(id, source) {
            Some(YamlNode::Alias(target)) => target,
            _ => id,
        };
        match graph.node(id, source) {
            YamlNode::Scalar { .. } => match schema::resolve_scalar(graph, id, dialect, source)? {
                crate::schema::ResolvedScalar::Core { tag, .. } => Ok(Cow::Borrowed(tag)),
                crate::schema::ResolvedScalar::Tagged { tag, .. } => Ok(Cow::Owned(tag)),
            },
            YamlNode::Sequence { tag, .. } => Ok(collection_tag(tag, TAG_SEQ)),
            YamlNode::Mapping { tag, .. } => Ok(collection_tag(tag, TAG_MAP)),
            YamlNode::Alias(_) => unreachable!("aliases are followed above"),
        }
    }
}

/// The scalar semantic comparison: tag-defined value.
fn scalar_equivalent(left: &str, right: &str, tag: Option<&str>) -> Verdict {
    match tag {
        Some(t) if is_standard_tag(t) => match t {
            TAG_STR | TAG_BOOL | TAG_MAP | TAG_SEQ | TAG_NULL => {
                if t == TAG_NULL {
                    Verdict::Equal
                } else {
                    str_eq(left, right)
                }
            }
            TAG_INT => int_eq(left, right),
            TAG_FLOAT => float_eq(left, right),
            // Unreachable: the guard restricts to the seven standard tags.
            _ => str_eq(left, right),
        },
        // Non-core tags: the exact payload text under the exact tag (the tag already matched above).
        _ => str_eq(left, right),
    }
}

fn str_eq(left: &str, right: &str) -> Verdict {
    if left == right {
        Verdict::Equal
    } else {
        Verdict::Different
    }
}

/// Integer equality by mathematical value (any radix spelling).
fn int_eq(left: &str, right: &str) -> Verdict {
    match (
        crate::document::canonical_integer_for(left),
        crate::document::canonical_integer_for(right),
    ) {
        (Some(l), Some(r)) if l == r => Verdict::Equal,
        _ => Verdict::Different,
    }
}

/// Float equality: numeric with `-0.0 == +0.0` and NaN == NaN.
fn float_eq(left: &str, right: &str) -> Verdict {
    // NaN spellings: every fixed YAML NaN equals every other.
    if is_nan_spelling(left) || is_nan_spelling(right) {
        return if is_nan_spelling(left) && is_nan_spelling(right) {
            Verdict::Equal
        } else {
            Verdict::Different
        };
    }
    // Infinities: signed.
    let l_inf = is_infinity_spelling(left);
    let r_inf = is_infinity_spelling(right);
    if l_inf || r_inf {
        if !(l_inf && r_inf) {
            return Verdict::Different;
        }
        return if is_negative_infinity(left) == is_negative_infinity(right) {
            Verdict::Equal
        } else {
            Verdict::Different
        };
    }
    // Finite spellings compare as EXACT decimals: a finite spelling keeps its exact magnitude instead of widening to
    // f64, so `1e400` stays a huge exact value (not +inf) and `1e-400` a tiny exact one (not 0.0) — the old
    // `parse::<f64>()` collapsed both into false equality. `Decimal::parse` already normalizes: leading/trailing zeroes
    // stripped (scale adjusted), and a zero magnitude of either sign collapses to the canonical "0" at scale 0, so `1.5
    // == 1.50` and `-0.0 == +0.0` fall out of the (coefficient, scale) pair. Underscores (legal in the core float
    // production) are stripped before the parse.
    let (Ok(l), Ok(r)) = (
        jqf_data::Decimal::parse(&crate::document::decode_float_spelling(left)),
        jqf_data::Decimal::parse(&crate::document::decode_float_spelling(right)),
    ) else {
        return Verdict::Different;
    };
    if l.coefficient().as_str() == r.coefficient().as_str() && l.scale() == r.scale() {
        Verdict::Equal
    } else {
        Verdict::Different
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
    use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef, Span};

    use crate::graph::{ScalarStyle, TextRef};

    fn resources<'a>() -> ResourceContext<'a> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("context")
    }

    fn source() -> ResolvedSource<'static> {
        ResolvedSource::new(SourceRef::new(SourceId::new(1), SourceKind::Input), "t", b"", 0)
    }

    fn span() -> Span {
        Span::try_from_usize(0, 0).unwrap_or_else(|_| unreachable!("zero span"))
    }

    fn scalar(graph: &mut YamlGraph, _resources: &mut ResourceContext<'_>, text: &str) -> NodeId {
        let text_id = graph.store_text(text);
        graph
            .add_scalar(TextRef::Owned(text_id), ScalarStyle::Plain as u8, None, None, span())
            .expect("node id in range")
    }

    fn mapping(graph: &mut YamlGraph, _resources: &mut ResourceContext<'_>, entries: &[(NodeId, NodeId)]) -> NodeId {
        let map = graph.add_mapping(None, None, span()).expect("node id in range");
        graph.close_mapping(map, entries).expect("close");
        map
    }

    /// A key mapping `{a: 1, b: N}` with plain untagged scalars.
    fn key_a1_bn(graph: &mut YamlGraph, resources: &mut ResourceContext<'_>, n: &str) -> NodeId {
        let a = scalar(graph, resources, "a");
        let one = scalar(graph, resources, "1");
        let b = scalar(graph, resources, "b");
        let n = scalar(graph, resources, n);
        mapping(graph, resources, &[(a, one), (b, n)])
    }

    fn compare(
        graph: &YamlGraph,
        src: ResolvedSource<'_>,
        resources: &ResourceContext<'_>,
        left: NodeId,
        right: NodeId,
    ) -> Verdict {
        KeyEquality::try_new(graph, src, crate::provider::DialectKind::Core)
            .expect("equality")
            .equals(left, right, resources)
            .expect("compare")
    }

    /// The livelock shape: the first left key matches a right key, the second does not. The old pop/re-scan backtracker
    /// spun forever here.
    #[test]
    fn match_entries_no_perfect_matching_is_different() {
        let mut resources = resources();
        let src = source();
        let mut graph = YamlGraph::try_new().expect("graph");
        let v = scalar(&mut graph, &mut resources, "v");
        let left_entries = vec![
            (key_a1_bn(&mut graph, &mut resources, "2"), v),
            (key_a1_bn(&mut graph, &mut resources, "3"), v),
        ];
        let left = mapping(&mut graph, &mut resources, &left_entries);
        let right_entries = vec![
            (key_a1_bn(&mut graph, &mut resources, "2"), v),
            (key_a1_bn(&mut graph, &mut resources, "4"), v),
        ];
        let right = mapping(&mut graph, &mut resources, &right_entries);
        let verdict = compare(&graph, src, &resources, left, right);
        assert_eq!(verdict, Verdict::Different);
    }

    /// A perfect matching exists but the right entries are permuted, so a greedy resume-at-previous+1 search would miss
    /// it; Kuhn's finds it.
    #[test]
    fn match_entries_permutation_matching_is_equal() {
        let mut resources = resources();
        let src = source();
        let mut graph = YamlGraph::try_new().expect("graph");
        let v = scalar(&mut graph, &mut resources, "v");
        let left_entries = vec![
            (key_a1_bn(&mut graph, &mut resources, "2"), v),
            (key_a1_bn(&mut graph, &mut resources, "3"), v),
        ];
        let left = mapping(&mut graph, &mut resources, &left_entries);
        let right_entries = vec![
            (key_a1_bn(&mut graph, &mut resources, "3"), v),
            (key_a1_bn(&mut graph, &mut resources, "2"), v),
        ];
        let right = mapping(&mut graph, &mut resources, &right_entries);
        let verdict = compare(&graph, src, &resources, left, right);
        assert_eq!(verdict, Verdict::Equal);
    }

    /// A scalar node carrying an explicit resolved tag (e.g. `!!float`).
    fn tagged_scalar(graph: &mut YamlGraph, _resources: &mut ResourceContext<'_>, text: &str, tag: &str) -> NodeId {
        let text_id = graph.store_text(text);
        let tag_id = graph.intern_name(tag).expect("name intern fits");
        graph
            .add_scalar(
                TextRef::Owned(text_id),
                ScalarStyle::Plain as u8,
                Some(tag_id),
                None,
                span(),
            )
            .expect("node id in range")
    }

    /// A `!!float`-tagged scalar node (a real float mapping key).
    fn float_scalar(graph: &mut YamlGraph, resources: &mut ResourceContext<'_>, text: &str) -> NodeId {
        tagged_scalar(graph, resources, text, TAG_FLOAT)
    }

    /// Finite float spellings are exact decimals, so out-of-range exact magnitudes must never collapse into f64 inf/0.0
    /// false equality, and equivalent spellings still agree.
    #[test]
    fn float_keys_compare_as_exact_decimals() {
        let mut resources = resources();
        let src = source();
        let mut graph = YamlGraph::try_new().expect("graph");

        // Distinct exact magnitudes: never duplicate keys.
        let l = float_scalar(&mut graph, &mut resources, "1e-400");
        let r = float_scalar(&mut graph, &mut resources, "0.0");
        assert_eq!(compare(&graph, src, &resources, l, r), Verdict::Different);

        let l = float_scalar(&mut graph, &mut resources, "1e-400");
        let r = float_scalar(&mut graph, &mut resources, "-0.0");
        assert_eq!(compare(&graph, src, &resources, l, r), Verdict::Different);

        let l = float_scalar(&mut graph, &mut resources, "1e400");
        let r = float_scalar(&mut graph, &mut resources, "1e4000");
        assert_eq!(compare(&graph, src, &resources, l, r), Verdict::Different);

        let l = float_scalar(&mut graph, &mut resources, "1e400");
        let r = float_scalar(&mut graph, &mut resources, ".inf");
        assert_eq!(compare(&graph, src, &resources, l, r), Verdict::Different);

        let l = float_scalar(&mut graph, &mut resources, "1.5");
        let r = float_scalar(&mut graph, &mut resources, "1.6");
        assert_eq!(compare(&graph, src, &resources, l, r), Verdict::Different);

        // Same exact value, different spellings: duplicate keys.
        let l = float_scalar(&mut graph, &mut resources, "1.5");
        let r = float_scalar(&mut graph, &mut resources, "1.50");
        assert_eq!(compare(&graph, src, &resources, l, r), Verdict::Equal);

        let l = float_scalar(&mut graph, &mut resources, "0.0");
        let r = float_scalar(&mut graph, &mut resources, "-0.0");
        assert_eq!(compare(&graph, src, &resources, l, r), Verdict::Equal);

        let l = float_scalar(&mut graph, &mut resources, "1e2");
        let r = float_scalar(&mut graph, &mut resources, "100.0");
        assert_eq!(compare(&graph, src, &resources, l, r), Verdict::Equal);

        let l = float_scalar(&mut graph, &mut resources, "0.5e1");
        let r = float_scalar(&mut graph, &mut resources, "5.0");
        assert_eq!(compare(&graph, src, &resources, l, r), Verdict::Equal);
    }

    /// Float and int kinds stay apart even at the same magnitude — `1e2` is a float and `100` an int, so they are
    /// distinct keys (the kind/tag law, never the scalar comparison).
    #[test]
    fn float_and_int_kinds_stay_apart() {
        let mut resources = resources();
        let src = source();
        let mut graph = YamlGraph::try_new().expect("graph");
        let float = tagged_scalar(&mut graph, &mut resources, "1e2", TAG_FLOAT);
        let int = tagged_scalar(&mut graph, &mut resources, "100", TAG_INT);
        assert_eq!(compare(&graph, src, &resources, float, int), Verdict::Different);
    }

    #[test]
    fn two_aliases_to_the_same_scalar_compare_equal() {
        let mut resources = resources();
        let src = source();
        let mut graph = YamlGraph::try_new().expect("graph");
        let target = scalar(&mut graph, &mut resources, "a");
        let left = graph.add_alias(target, span()).expect("alias");
        let right = graph.add_alias(target, span()).expect("alias");
        assert_eq!(compare(&graph, src, &resources, left, right), Verdict::Equal);
    }
}
