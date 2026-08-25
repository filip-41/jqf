//! YAML graph navigation: exact-path resolution over the codec's graph arena.
//!
//! The route sessions (scoped) share this walk: validate the whole input through the ordinary scanner+parser into the
//! graph (validate-everything-first), then resolve the exact path over the graph with the same member/signed-index
//! semantics the whole-document interpreter uses. Negative observations are `Missing`/`TypeMismatch`, exactly like the
//! floor.

use alloc::borrow::ToOwned;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::{CodecError, CodecFailureKind, PortableStep};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use jqf_data::ValueKind;

use crate::document::fingerprint;
use crate::graph::{NodeId, YamlGraph, YamlNode};
use crate::key::{KeyEquality, Verdict};
use crate::provider::DialectKind;
use crate::schema::{self, ResolvedScalar, ScalarCategory};

/// The located answer of a graph navigation.
#[derive(Clone, Debug)]
pub(crate) enum Located {
    /// A node at the exact path.
    Node(NodeId),
    /// A contiguous range of a sequence, materialized as a FRESH array of the selected elements (the
    /// slice-materialization law).
    Range {
        /// The selected elements, in order.
        items: Vec<NodeId>,
    },
    /// The path stepped past the end of a container.
    Missing {
        /// The step index that failed.
        step: usize,
    },
    /// A step applied to a container of the wrong kind.
    TypeMismatch {
        /// The step index that failed.
        step: usize,
        /// The kind of the value the step was applied TO.
        ///
        /// It is load-bearing, not decoration: the engine reads it to decide whether the mismatch raises at all,
        /// because `null` is the one kind a member step may index. Publishing a fixed `Null` here — which the scoped
        /// route did before this field existed — makes every mismatch look legal and answers `null` where the
        /// whole-document floor raises `Cannot index array with string`.
        actual: ValueKind,
    },
}

/// Owns one portable step's text (the member name).
#[derive(Debug)]
pub(crate) enum OwnedStep {
    Member(Vec<u8>),
    Index(i64),
    /// A contiguous signed range `[start, end)` of an array container.
    Range {
        /// Lower bound (open at the container's start when `None`).
        start: Option<i64>,
        /// Upper bound (open at the container's end when `None`).
        end: Option<i64>,
    },
}

/// Copies portable steps into session-owned storage.
pub(crate) fn own_steps(steps: &[PortableStep]) -> Result<Vec<OwnedStep>, CodecError> {
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(steps.len())
        .map_err(jqf_resource::ResourceError::from)?;
    for (position, step) in steps.iter().enumerate() {
        // A range is materialized as a FRESH array and this walk cannot carry navigation past it, so a non-trailing
        // range would otherwise be answered with every later step silently DISCARDED (`.a[1:3].b` as `.a[1:3]`). The
        // engine's range-locate row lowers only a trailing slice; anything else declines the route, which the SDK maps
        // to the whole-document floor rather than failing the request.
        if position + 1 < steps.len() && matches!(step, PortableStep::SemanticRange { .. }) {
            return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
        }
        let step = match step {
            PortableStep::SemanticMember(name) => {
                let name = name.as_str();
                let mut text = Vec::new();
                text.try_reserve_exact(name.len())
                    .map_err(jqf_resource::ResourceError::from)?;
                for byte in name.as_bytes() {
                    text.push(*byte);
                }
                OwnedStep::Member(text)
            }
            PortableStep::SemanticIndex(index) => OwnedStep::Index(*index),
            PortableStep::SemanticRange { start, end } => OwnedStep::Range {
                start: *start,
                end: *end,
            },
        };
        owned.push(step);
    }
    Ok(owned)
}

/// Resolves an exact path over the graph, returning the located node or a negative observation.
///
/// `dialect` is here for one reason: a `TypeMismatch` must report the kind of the value the step was applied to, and a
/// YAML SCALAR's kind is a schema question (`null`, `true`, `1`, `x` are four kinds of one node shape) that only
/// [`crate::schema::resolve_scalar`] can answer.
pub(crate) fn locate(
    graph: &YamlGraph,
    steps: &[OwnedStep],
    source: ResolvedSource<'_>,
    dialect: DialectKind,
) -> Result<Located, CodecError> {
    let Some(root) = graph_root(graph) else {
        return Ok(Located::Missing { step: 0 });
    };
    let mut current = graph.follow_alias(root, source)?;
    // Empty steps navigate to the ROOT: a mapping root holding a non-core-string key must raise (keys/type on `{1: 42}`
    // answer identity today, which the floor refuses) — same law as the Member arm above. A NON-mapping root (array,
    // scalar) is returned as-is: empty steps always answer the root.
    let root_mapping = match graph.node(current, source) {
        YamlNode::Mapping { entries, .. } => Some(entries),
        _ => None,
    };
    if let (true, Some(entries)) = (steps.is_empty(), root_mapping) {
        validate_mapping_keys(graph, entries, source, dialect)?;
    }
    for (index, step) in steps.iter().enumerate() {
        current = graph.follow_alias(current, source)?;
        let Some(node) = graph.node_opt(current, source) else {
            return Ok(Located::Missing { step: index });
        };
        let next = match (node, step) {
            (YamlNode::Mapping { entries, .. }, OwnedStep::Member(name)) => {
                // The mapping-keys law applies on EVERY rung: a non-core-string key is never coerced, so a member step
                // over a mapping that holds one must raise the floor's yaml.key error — never answer Missing/null
                // (scoped route) or fabricate identity.
                validate_mapping_keys(graph, entries, source, dialect)?;
                let name = String::from_utf8_lossy(name.as_slice());
                // LAST-VALUE-WINS: scan the ENTIRE mapping and keep the final matching entry. This is the same law
                // materialization keeps (the object builder lets the final occurrence supply the value), so located
                // navigation and materialization answer the same document the same way; first-hit-stop would answer the
                // FIRST occurrence and disagree with `.[0]`'s object.
                let mut found = None;
                for (key, value) in entries {
                    if key_text(graph, *key, source).as_deref() == Some(name.as_ref()) {
                        found = Some(*value);
                    }
                }
                match found {
                    Some(value) => value,
                    None => return Ok(Located::Missing { step: index }),
                }
            }
            (YamlNode::Sequence { items, .. }, OwnedStep::Index(raw)) => {
                let len = items.len();
                let position = resolve_index(*raw, len);
                match position {
                    Some(pos) if pos < len => items[pos],
                    _ => return Ok(Located::Missing { step: index }),
                }
            }
            (YamlNode::Sequence { items, .. }, OwnedStep::Range { start, end }) => {
                let len = items.len();
                // Slice clamps both bounds to the container's edges; [`resolve_index`] answers None exactly when a
                // bound sits past one, so each failure resolves by the AUTHORED sign: a positive bound clamps to the
                // far edge, a negative one to the head. A sign-blind fallback publishes the whole array (or its tail)
                // where the floor answers [] — `.a[5:]`, `.[:-5]`, `.a[1:-10]`.
                let range_start = match start {
                    Some(bound) => resolve_index(*bound, len).unwrap_or(if *bound >= 0 { len } else { 0 }),
                    None => 0,
                };
                let range_end = match end {
                    Some(bound) => resolve_index(*bound, len).unwrap_or(if *bound >= 0 { len } else { 0 }),
                    None => len,
                };
                if range_start >= range_end {
                    return Ok(Located::Range { items: Vec::new() });
                }
                let selected: Vec<NodeId> = items
                    .get(range_start..range_end.min(len))
                    .map(<[NodeId]>::to_vec)
                    .unwrap_or_default();
                return Ok(Located::Range { items: selected });
            }
            _ => {
                return Ok(Located::TypeMismatch {
                    step: index,
                    actual: node_kind(graph, current, dialect, source)?,
                });
            }
        };
        current = next;
    }
    Ok(Located::Node(current))
}

/// The published kind of one graph node, as the whole-document route would materialize it.
///
/// A sequence and a mapping answer straight from the node shape. A scalar does not: `null`, `true`, `1`, `1.5` and `x`
/// are all the same node shape and differ only under the schema, so this defers to [`crate::schema::resolve_scalar`] —
/// the same resolution the materializer uses, which is what keeps the mismatch's reported kind identical to the
/// floor's. A NON-CORE tag publishes the kind of its payload, and a scalar the failsafe schema could not resolve
/// reports `String`, the category it publishes under.
fn node_kind(
    graph: &YamlGraph,
    node: NodeId,
    dialect: DialectKind,
    source: ResolvedSource<'_>,
) -> Result<ValueKind, CodecError> {
    match graph.node(node, source) {
        YamlNode::Sequence { .. } => Ok(ValueKind::Array),
        YamlNode::Mapping { .. } => Ok(ValueKind::Object),
        // An alias is followed before any step is applied, so it never reaches a mismatch — resolve through it rather
        // than inventing a kind.
        YamlNode::Alias(target) => node_kind(graph, target, dialect, source),
        YamlNode::Scalar { .. } => {
            let category = match schema::resolve_scalar(graph, node, dialect, source)? {
                ResolvedScalar::Core { category, .. } => category,
                ResolvedScalar::Tagged { payload, .. } => payload,
            };
            Ok(match category {
                ScalarCategory::Null => ValueKind::Null,
                ScalarCategory::Bool(_) => ValueKind::Bool,
                ScalarCategory::Integer | ScalarCategory::Float => ValueKind::Number,
                ScalarCategory::String => ValueKind::String,
            })
        }
    }
}

/// Resolves a signed array index (negative wraps from the end), like the engine's `.[i]`. The law is
/// [`jqf_data::resolve_index`]; this is the codec's internal name for it, with the codec's historical argument order.
fn resolve_index(index: i64, len: usize) -> Option<usize> {
    jqf_data::resolve_index(len, index)
}

/// The graph root: the parser-recorded document root.
pub(crate) fn graph_root(graph: &YamlGraph) -> Option<NodeId> {
    graph.root()
}

/// The core-string key text of a mapping key, when it is one.
pub(crate) fn key_text(graph: &YamlGraph, key: NodeId, source: ResolvedSource<'_>) -> Option<String> {
    match graph.node(key, source) {
        YamlNode::Scalar { text, .. } => Some(text.to_owned()),
        _ => None,
    }
}

/// The BORROWED core-string key text of a mapping key, when it is one. The key-comparison hot loops
/// (projected/structure element scans) call this instead of [`key_text`] to avoid a per-key `String` allocation.
#[must_use]
pub(crate) fn key_text_ref<'a>(
    graph: &'a YamlGraph,
    key: NodeId,
    source: jqf_source::ResolvedSource<'a>,
) -> Option<&'a str> {
    match graph.node(key, source) {
        YamlNode::Scalar { text, .. } => Some(text),
        _ => None,
    }
}

/// Whether a mapping key is a direct core String: quoted, explicit `!!str`, an EMPTY plain scalar (the corpus's `: a`
/// empty-key reading), or a plain scalar that resolves to String under the dialect. A complex or non-core-tagged key is
/// never coerced (AGENTS.md's mapping-keys law); a `!`-tagged scalar resolves to String per the spec, so it IS a key.
fn is_core_string_key(
    graph: &YamlGraph,
    key: NodeId,
    dialect: DialectKind,
    source: ResolvedSource<'_>,
) -> Result<bool, CodecError> {
    let node = graph.node(key, source);
    let YamlNode::Scalar { text, style, .. } = node else {
        return Ok(false);
    };
    if style != crate::graph::ScalarStyle::Plain {
        return Ok(true); // quoted: always a string
    }
    if text.is_empty() {
        return Ok(true); // empty plain mapping key is the empty string
    }
    Ok(matches!(
        crate::schema::resolve_scalar(graph, key, dialect, source)?,
        crate::schema::ResolvedScalar::Core {
            category: crate::schema::ScalarCategory::String,
            ..
        }
    ))
}

/// Validates every key of a mapping under the core-string law, raising the floor's exact `yaml.key` error on the first
/// violation (scan ALL entries; never early-break on a found member).
fn validate_mapping_keys(
    graph: &YamlGraph,
    entries: &[(NodeId, NodeId)],
    source: ResolvedSource<'_>,
    dialect: DialectKind,
) -> Result<(), CodecError> {
    for (key, _) in entries {
        if is_core_string_key(graph, *key, dialect, source)? {
            continue;
        }
        return Err(non_string_key_error(graph, *key, source));
    }
    Ok(())
}

/// The floor's `yaml.key` refusal: class, span, and message.
pub(crate) fn non_string_key_error(graph: &YamlGraph, key: NodeId, source: ResolvedSource<'_>) -> CodecError {
    let message = match graph.node(key, source) {
        YamlNode::Scalar { .. } => "a non-string mapping key is not coerced to an object key",
        YamlNode::Alias(_) => "an aliased mapping key is not coerced to an object key",
        _ => "a complex mapping key is not coerced to an object key",
    };
    let span = graph.node_span(key);
    crate::error::unsupported(source, span.start() as usize, span.end() as usize, "key", message)
}

/// The whole-graph duplicate-key validation: walks every node reachable from the document root and enforces
/// `yaml.key-equivalence@1` on every mapping — the same law the whole-document build's `build_mapping` phase 1
/// enforces, run in the SHARED validation phase (`GraphParse`) so the scoped route rejects a duplicate-key document
/// exactly like the floor. Validate-everything-first owns the parity: a duplicate in a field the program never reads
/// must fail the fast routes too, or a pushed-down program publishes bytes the floor refuses.
///
/// A non-core-string key is a `yaml.key` failure on this same walk — the floor refuses it, so every fast route must
/// too. A key the object projection would never hold is therefore never a duplicate candidate.
///
/// The walk is O(nodes): one visit per reachable node (an alias shares its target once), one fingerprint shortlist per
/// mapping, and the frozen comparator only on a fingerprint hit — mirroring the whole-document build's own phase-1
/// cost.
pub(crate) fn validate_duplicate_keys(
    graph: &YamlGraph,
    source: ResolvedSource<'_>,
    dialect: DialectKind,
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    let Some(root) = graph_root(graph) else {
        return Ok(());
    };
    let mut visited: Vec<bool> = Vec::new();
    let mut equality = KeyEquality::try_new(graph, source, dialect)?;
    validate_node_keys(graph, root, source, dialect, resources, &mut visited, &mut equality)
}

/// One depth-first step of the duplicate-key walk. The visited mark is set BEFORE descending: an alias re-entering a
/// node still on the walk path is a graph CYCLE (the graph retains cycles; only the semantic build refuses them), and
/// the mark is what stops the walk from looping on it.
fn validate_node_keys(
    graph: &YamlGraph,
    node: NodeId,
    source: ResolvedSource<'_>,
    dialect: DialectKind,
    resources: &mut ResourceContext<'_>,
    visited: &mut Vec<bool>,
    equality: &mut KeyEquality<'_>,
) -> Result<(), CodecError> {
    let index = node.index();
    while visited.len() <= index {
        visited.push(false);
    }
    if visited[index] {
        return Ok(());
    }
    visited[index] = true;
    // The nesting guard: the walk recurses once per container level, the same accounted depth the whole-document build
    // keeps, so the 10000-level ceiling is a clean codec error here too.
    let _depth = resources.enter_nesting_owned().map_err(CodecError::from)?;
    match graph.node_opt(node, source) {
        None => Err(CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "YAML duplicate-key walk over a missing node",
        })),
        Some(YamlNode::Scalar { .. } | YamlNode::Alias(..)) => Ok(()),
        Some(YamlNode::Sequence { items, .. }) => {
            for item in items {
                validate_node_keys(graph, *item, source, dialect, resources, visited, equality)?;
            }
            Ok(())
        }
        Some(YamlNode::Mapping { entries, .. }) => {
            validate_mapping_keys(graph, entries, source, dialect)?;
            check_mapping_duplicates(graph, entries, source, dialect, resources, equality)?;
            for (key, value) in entries {
                // A complex key is itself a container: its nested mappings carry the same law (the floor walks them).
                validate_node_keys(graph, *key, source, dialect, resources, visited, equality)?;
                validate_node_keys(graph, *value, source, dialect, resources, visited, equality)?;
            }
            Ok(())
        }
    }
}

/// One mapping's duplicate-key check: the exact mirror of the whole-document build's phase-1 walk. The fingerprint
/// shortlist holds only direct core-string keys (a non-core-string key is never a duplicate candidate), and a
/// fingerprint hit still runs the frozen comparator, which stays the decision-maker. The buckets CHAIN their occupants,
/// exactly like the whole-document build's: a single-slot map would let two different texts sharing a fingerprint
/// overwrite each other and hide an exact duplicate.
fn check_mapping_duplicates(
    graph: &YamlGraph,
    entries: &[(NodeId, NodeId)],
    source: ResolvedSource<'_>,
    dialect: DialectKind,
    resources: &ResourceContext<'_>,
    equality: &mut KeyEquality<'_>,
) -> Result<(), CodecError> {
    let mut seen_text: BTreeMap<u64, Vec<NodeId>> = BTreeMap::new();
    for (key, _value) in entries {
        if !is_core_string_key(graph, *key, dialect, source)? {
            continue;
        }
        let Some(text) = key_text_ref(graph, *key, source) else {
            continue;
        };
        if let Some(occupants) = seen_text.get(&fingerprint(text)) {
            for &previous in occupants {
                if equality.equals(previous, *key, resources)? == Verdict::Equal {
                    let span = graph.node_span(*key);
                    return Err(crate::error::invalid_range(
                        source,
                        span.start() as usize,
                        span.end() as usize,
                        "duplicate-key",
                        "mapping key is a duplicate under yaml.key-equivalence@1",
                    ));
                }
            }
        }
        seen_text.entry(fingerprint(text)).or_default().push(*key);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceLimits, WorkMeter};
    use jqf_source::{SourceId, SourceKind, SourceRef};

    fn resources<'a>() -> ResourceContext<'a> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("context")
    }

    fn source(bytes: &'static [u8]) -> ResolvedSource<'static> {
        ResolvedSource::new(
            SourceRef::new(SourceId::new(1), SourceKind::Input),
            "test.yaml",
            bytes,
            0,
        )
    }

    fn graph_parse(bytes: &'static [u8]) -> Result<YamlGraph, CodecError> {
        let src = source(bytes);
        let mut resources = resources();
        let mut parse = crate::scoped::GraphParse::try_new(src, DialectKind::Core, &resources).expect("parse");
        loop {
            match parse.poll(src, &mut resources)? {
                crate::scoped::GraphParsePoll::Pending => {}
                crate::scoped::GraphParsePoll::Ready(graph) => return Ok(*graph),
            }
        }
    }

    #[test]
    fn unread_non_string_key_fails_shared_validation() {
        let Err(error) = graph_parse(b"sel: x\nother:\n  1: bad\n") else {
            panic!("unread non-string key must fail shared validation")
        };
        assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
    }

    #[test]
    fn string_keys_still_pass_shared_validation() {
        if let Err(error) = graph_parse(b"sel: x\nother:\n  k: ok\n") {
            panic!("string keys must pass, got {error:?}");
        }
    }

    const THREE_ITEMS: &[u8] = b"a:\n  - x\n  - y\n  - z\n";

    const ROOT_ARRAY: &[u8] = b"- x\n- y\n- z\n";

    fn member(name: &str) -> PortableStep {
        PortableStep::SemanticMember(alloc::string::String::from(name))
    }

    /// Runs one exact path over a parsed fixture and unwraps the located answer.
    fn locate_steps(bytes: &'static [u8], steps: &[PortableStep]) -> Located {
        let graph = graph_parse(bytes).expect("parse");
        let owned = own_steps(steps).expect("own steps");
        locate(&graph, &owned, source(bytes), DialectKind::Core).expect("locate")
    }

    #[test]
    fn positive_start_past_end_clamps_to_empty() {
        // Floor law: `.a[5:]` over three items clamps the start to the far edge and answers []. A sign-blind fallback
        // answered the WHOLE array.
        let located = locate_steps(
            THREE_ITEMS,
            &[
                member("a"),
                PortableStep::SemanticRange {
                    start: Some(5),
                    end: None,
                },
            ],
        );
        let Located::Range { items } = located else {
            panic!("expected a range, got {located:?}");
        };
        assert!(items.is_empty());
    }

    #[test]
    fn negative_end_past_head_clamps_to_empty() {
        // `.[:-5]` over three items: the end wraps past the head and clamps to 0, so the answer is [] — not the whole
        // array.
        let located = locate_steps(
            ROOT_ARRAY,
            &[PortableStep::SemanticRange {
                start: None,
                end: Some(-5),
            }],
        );
        let Located::Range { items } = located else {
            panic!("expected a range, got {located:?}");
        };
        assert!(items.is_empty());
    }

    #[test]
    fn negative_end_past_head_truncates_tail() {
        // `.a[1:-10]`: the tail collapses to [] where the old fallback answered the whole tail.
        let located = locate_steps(
            THREE_ITEMS,
            &[
                member("a"),
                PortableStep::SemanticRange {
                    start: Some(1),
                    end: Some(-10),
                },
            ],
        );
        let Located::Range { items } = located else {
            panic!("expected a range, got {located:?}");
        };
        assert!(items.is_empty());
    }

    #[test]
    fn negative_start_past_head_keeps_whole_array() {
        // `.a[-5:]` over three items clamps the start to the head edge: the whole array, exactly the kept pre-fix
        // behavior.
        let located = locate_steps(
            THREE_ITEMS,
            &[
                member("a"),
                PortableStep::SemanticRange {
                    start: Some(-5),
                    end: None,
                },
            ],
        );
        let Located::Range { items } = located else {
            panic!("expected a range, got {located:?}");
        };
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn resolvable_bounds_unchanged() {
        let located = locate_steps(
            THREE_ITEMS,
            &[
                member("a"),
                PortableStep::SemanticRange {
                    start: Some(1),
                    end: Some(-1),
                },
            ],
        );
        let Located::Range { items } = located else {
            panic!("expected a range, got {located:?}");
        };
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn mid_path_range_declines_instead_of_discarding() {
        // `.a[1:3].b`: a non-trailing range would silently drop every later step, so owning the path must refuse it.
        // RequirementMismatch is the kind the SDK maps to the whole-document floor.
        let error = own_steps(&[
            member("a"),
            PortableStep::SemanticRange {
                start: Some(1),
                end: Some(3),
            },
            member("b"),
        ])
        .expect_err("mid-path range must decline");
        assert_eq!(error.kind(), CodecFailureKind::RequirementMismatch);
    }

    #[test]
    fn trailing_range_still_owned() {
        let owned = own_steps(&[
            member("a"),
            PortableStep::SemanticRange {
                start: Some(1),
                end: Some(3),
            },
        ])
        .expect("trailing range owns");
        assert_eq!(owned.len(), 2);
    }

    #[test]
    fn alias_is_followed_before_a_member_step() {
        // An alias is not a consumed path step: `.copy.x` when `copy: *b` must land on the anchored mapping's `x`, not
        // the mapping itself.
        let located = locate_steps(b"b: &b {x: 1}\ncopy: *b\n", &[member("copy"), member("x")]);
        let Located::Node(_) = located else {
            panic!("expected a node, got {located:?}");
        };
    }

    #[test]
    fn member_step_on_an_aliased_scalar_is_a_type_mismatch() {
        let located = locate_steps(b"b: &b 1\ncopy: *b\n", &[member("copy"), member("x")]);
        let Located::TypeMismatch { step, actual } = located else {
            panic!("expected type mismatch, got {located:?}");
        };
        assert_eq!(step, 1);
        assert_eq!(actual, ValueKind::Number);
    }
}
