//! YAML semantic document construction (whole-document route).
//!
//! Walks the codec's graph arena and builds the format-neutral [`AccountedDocumentBuilder`] document: scalars resolve
//! through the selected schema (failsafe/JSON/core), sequences project to arrays, mappings whose keys are all direct
//! core strings project to objects, and resolved/non-core tags attach through [`AccountedIntrinsicTag`]. Alias
//! occurrences reference the SAME document node (sharing preserved); a cyclic graph cannot become a semantic value and
//! fails with `UnsupportedRepresentation` (the graph itself retains the cycle).
//!
//! Duplicate-key validation runs HERE, before object projection, under `yaml.key-equivalence@1`. Only direct
//! core-string keys reach that comparator; a non-string key refuses projection instead.

use alloc::borrow::ToOwned;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::{CodecError, CodecFailureKind};

pub(crate) use jqf_codec_core::{PRUNE_ALL, PruneLookup, PruneRef};
use jqf_data::{
    AccountedDocumentBuilder, AccountedIntrinsicTag, AccountedOccurrenceKey, AccountedSemanticNode, BuilderCoverage,
    DataError, DocumentCapacity, DocumentSchemaRecipe, FactPayload, LocalOwnerRef, NodeId, PreparedDocumentSchema,
    ValueKind,
};
use jqf_resource::ResourceContext;
use jqf_source::{ResolvedSource, Span};

use crate::error;
use crate::graph::{NodeId as GraphNode, ScalarStyle, YamlGraph, YamlNode};
use crate::key::{KeyEquality, Verdict};
use crate::provider::DialectKind;
use crate::schema::{self, ScalarCategory, TAG_MAP, TAG_SEQ, TAG_STR};

pub(crate) const SCALAR_KIND: &str = "yaml.scalar@1";
pub(crate) const SEQ_KIND: &str = "yaml.seq@1";
pub(crate) const MAP_KIND: &str = "yaml.map@1";
pub(crate) const ITEM_ROLE: &str = "yaml.item@1";
pub(crate) const MEMBER_ROLE: &str = "yaml.member@1";
/// The cross-format comment fact: one list-payload fact per node whose leading comments the whole-document walker
/// attached. Serves `.@comment`. The spelling is the shared vocabulary's `HEAD` segment under this codec's namespace;
/// the `comment_roles_agree_with_the_shared_vocabulary` test pins it against the builder.
pub(crate) const COMMENT_FACT: &str = "yaml.comment@1";
/// The comment after a value on the SAME line as that value: one list-payload fact on the value's node. Serves
/// `.@comment_inline`. Previously absorbed into the NEXT node's leading list; it now attaches to the node whose line
/// carries it.
pub(crate) const COMMENT_INLINE_FACT: &str = "yaml.comment_inline@1";
/// The comment lines below a closing block that belong to that block, not to a following sibling: one list-payload fact
/// on the closing collection. The document trailer is the ROOT's foot. Serves `.@comment_foot`.
pub(crate) const COMMENT_FOOT_FACT: &str = "yaml.comment_foot@1";
/// The prose a refusal fact carries: an edit through an alias refuses, because rewriting the anchor's authored span
/// would silently change every other alias site. The fact's ROLE is the format-neutral
/// [`jqf_codec_core::EDIT_REFUSAL_ROLE`]; this text is its payload.
pub(crate) const ALIAS_REFUSAL_MESSAGE: &str = "editing through an alias is refused: the value is referenced by an alias, \
     and rewriting its authored span would silently change every other alias site";
/// The anchor NAME of an anchored node: one text-payload fact per node that carries `&name` in the authored document.
/// The block encoder reads it so a floor re-emits `&name` at the anchor's own position and `*name` at every alias site
/// instead of flattening the document — the anchored-file-survives-a-floor hole this fact closes. The name is the
/// graph's interned exact text, never a synthesized spelling.
pub(crate) const ANCHOR_FACT: &str = "yaml.anchor@1";
/// The authored scalar STYLE of a scalar node: one text-payload fact per scalar recording how its content was spelled
/// (`plain`, `single`, `double`, `literal`, `folded` — the graph's [`ScalarStyle`] names). Serves `.@style` reads and
/// the edit lane's style-write verification: a `.@style` write re-renders the node's span, and the verify re-decodes
/// and compares this fact to the written payload.
pub(crate) const STYLE_FACT: &str = "yaml.style@1";

fn yaml_schema_recipe() -> Result<DocumentSchemaRecipe<'static>, DataError> {
    DocumentSchemaRecipe::try_new(
        "yaml",
        Some("yaml"),
        &[SCALAR_KIND, SEQ_KIND, MAP_KIND],
        &[ITEM_ROLE, MEMBER_ROLE],
        &[
            COMMENT_FACT,
            COMMENT_INLINE_FACT,
            COMMENT_FOOT_FACT,
            jqf_codec_core::EDIT_REFUSAL_ROLE,
            ANCHOR_FACT,
            STYLE_FACT,
        ],
        &[
            COMMENT_FACT,
            COMMENT_INLINE_FACT,
            COMMENT_FOOT_FACT,
            ANCHOR_FACT,
            STYLE_FACT,
            jqf_codec_core::MERGE_OVERRIDE_ROLE,
        ],
    )
}

pub(crate) fn map_data(error: DataError) -> CodecError {
    jqf_codec_core::map_data(error, "YAML builder rejected document construction")
}

pub(crate) fn data_contract() -> CodecError {
    jqf_codec_core::data_contract("YAML authoritative document construction")
}

/// A separation byte that may follow a plain scalar's value inside its token: the scanner consumes blanks and line
/// breaks after the value. The multi-byte break sequences (U+0085, U+2028, U+2029) are matched by their own bytes,
/// which can never close a valid UTF-8 scalar's content.
fn is_plain_separation(byte: u8) -> bool {
    matches!(
        byte,
        b' ' | b'\t' | b'\n' | b'\r' | 0x85 | 0xA8 | 0xA9 | 0xC2 | 0xE2 | 0x80
    )
}

/// Trims one trailing line break from a byte range end, matching the scanner's break set (`\n`, `\r`, U+0085, U+2028,
/// U+2029 — the last two as their 2/3-byte UTF-8 forms). The block-scalar span trim uses this so a spliced replacement
/// keeps the line terminator.
fn trim_trailing_break(bytes: &[u8], mut end: usize) -> usize {
    loop {
        if end == 0 {
            return 0;
        }
        match bytes[end - 1] {
            b'\n' | b'\r' => end -= 1,
            0x85 if end >= 2 && bytes[end - 2] == 0xC2 => end -= 2,
            0xA8 | 0xA9 if end >= 3 && bytes[end - 3] == 0xE2 && bytes[end - 2] == 0x80 => end -= 3,
            _ => return end,
        }
    }
}

/// Whether every byte after a plain scalar's value in its token is the scanner's trailing material: separation and line
/// breaks before any `#`, and then the inline comment itself — the comment runs to (but never past) the line break, so
/// a break after the comment is not part of the token and the value's span commits exactly at the comment's `#`.
fn trailing_plain_material(rest: &[u8]) -> bool {
    let mut in_comment = false;
    for &byte in rest {
        if in_comment {
            // The comment's own multi-byte break sequences cannot appear either: the scanner stops the comment at the
            // first break.
            if matches!(byte, b'\n' | b'\r' | 0x85 | 0xA8 | 0xA9) {
                return false;
            }
        } else if byte == b'#' {
            in_comment = true;
        } else if !is_plain_separation(byte) {
            return false;
        }
    }
    true
}

/// Builds the semantic document from the graph. The root is the parser's authoritative root node (the graph arena alone
/// cannot name it: the LAST node added is a scalar deep inside a mapping, not the root).
///
/// The returned `spans_committed` flag tells the caller whether the build admitted any bound-source span, which decides
/// whether the source must be sealed and bound before the document can finish — the YAML analog of the TOML/JSON seal
/// step. `decoded` is the session's decoded-source view: a span may be committed against the SOURCE only when the
/// decoded text the graph spans address IS the source's own bytes (UTF-8 input); a UTF-16/32 input's decoded buffer is
/// not the source, so no span is committed and an edit of such a document declines to the whole-document floor.
#[allow(
    clippy::too_many_arguments,
    reason = "the walk takes the graph, source, prune, and the two demand flags together"
)]
pub(crate) fn build_document(
    graph: &YamlGraph,
    root: GraphNode,
    source: ResolvedSource<'_>,
    dialect: DialectKind,
    decoded: &crate::scan::DecodedSource,
    prune: Option<&PruneLookup>,
    coverage: BuilderCoverage,
    want_tags: bool,
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId, bool), CodecError> {
    let mut walker = Walker::new(graph, source, dialect, decoded, prune, coverage, want_tags, resources)?;
    let root = walker.build_node(
        root,
        walker
            .prune
            .as_ref()
            .map_or(PRUNE_ALL, |_| jqf_codec_core::PruneTree::ROOT),
        resources,
    )?;
    // The whole-document walk attaches comments once, so the index is built here rather than threaded through; only
    // comment-free graphs skip it. Coverage without attached facts skips every fact attacher.
    if coverage.attached_facts() {
        let comment_index = (!graph.comments().is_empty()).then(|| CommentIndex::from_graph(graph));
        attach_comment_facts(
            walker.builder.as_mut().expect("builder present"),
            comment_index.as_ref(),
            &walker.memo,
            source,
            decoded.maps_to_source(),
            resources,
        )?;
        attach_alias_refusal_facts(
            walker.builder.as_mut().expect("builder present"),
            graph,
            &walker.memo,
            resources,
        )?;
        attach_anchor_facts(
            walker.builder.as_mut().expect("builder present"),
            graph,
            &walker.memo,
            source,
            resources,
        )?;
        attach_style_facts(
            walker.builder.as_mut().expect("builder present"),
            graph,
            &walker.memo,
            source,
            resources,
        )?;
        attach_merge_override_facts(
            walker.builder.as_mut().expect("builder present"),
            graph,
            &walker.memo,
            resources,
        )?;
    }
    Ok((
        walker.builder.take().expect("builder present"),
        root,
        walker.spans_committed,
    ))
}

fn fresh_builder(
    coverage: BuilderCoverage,
    _resources: &ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, PreparedDocumentSchema), CodecError> {
    let recipe = yaml_schema_recipe().map_err(map_data)?;
    let (mut builder, schema) =
        AccountedDocumentBuilder::try_new_prepared_with_coverage(&recipe, coverage).map_err(map_data)?;
    builder.set_authoritative_empty_families(jqf_data::AuthoritativeEmptyFamilies::from_family(
        jqf_data::DocumentCapabilityFamily::Attributes,
    ));
    builder.set_diagnostic_coverage(jqf_data::DiagnosticCoverage::NotRequested);
    Ok((builder, schema))
}

/// The graph-to-document walker.
struct Walker<'graph> {
    builder: Option<AccountedDocumentBuilder<'static>>,
    /// The prepared schema the builder was constructed with, used to commit bound-source scalar spans.
    schema: PreparedDocumentSchema,
    graph: &'graph YamlGraph,
    source: ResolvedSource<'graph>,
    dialect: DialectKind,
    /// Whether the decoded text the graph's spans address IS the source's own bytes (UTF-8 input): only then may a span
    /// be committed against the bound source. A UTF-16/32 input's decoded buffer is not the source, so no span is
    /// committed and the edit lane declines to the floor.
    source_mapped: bool,
    /// Whether any bound-source span was committed (decides whether the source must be sealed and bound before the
    /// document can finish).
    spans_committed: bool,
    /// Graph node -> document node memo (aliases share the document node).
    memo: Vec<Option<NodeId>>,
    /// Graph nodes on the current walk path (cycle detection).
    in_progress: Vec<GraphNode>,
    /// The key-equivalence comparator (duplicate-key validation).
    equality: KeyEquality<'graph>,
    /// The armed prune hint: which mapping members the requesting program provably reads. `None` keeps everything.
    prune: Option<PruneLookup>,
    /// Graph nodes referenced by at least one alias. An alias-shared node is built WHOLE wherever it is first reached,
    /// so the memo can serve it to every alias site without ever under-delivering a demand: the tree is position-based,
    /// two positions may alias the same node with different demands, and the memo keeps only the first build.
    alias_shared: Vec<bool>,
    /// See [`demanded_intrinsic`].
    want_tags: bool,
}

impl<'graph> Walker<'graph> {
    #[allow(
        clippy::too_many_arguments,
        reason = "the walk takes the graph, source, prune, and the two demand flags together"
    )]
    fn new(
        graph: &'graph YamlGraph,
        source: ResolvedSource<'graph>,
        dialect: DialectKind,
        decoded: &crate::scan::DecodedSource,
        prune: Option<&PruneLookup>,
        coverage: BuilderCoverage,
        want_tags: bool,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        let (mut builder, schema) = fresh_builder(coverage, resources)?;
        let _ = builder.try_reserve(
            DocumentCapacity {
                nodes: graph.len(),
                occurrences: graph.occurrence_count(),
                facts: if coverage.attached_facts() {
                    graph.comments().len().saturating_add(graph.merge_hosts().len())
                } else {
                    0
                },
                ..DocumentCapacity::default()
            },
            resources,
        );
        let equality = KeyEquality::new(graph, source, dialect);
        let mut alias_shared = alloc::vec![false; graph.len()];
        for target in graph.alias_targets() {
            alias_shared[target.index()] = true;
        }
        for (value, _) in graph.merge_hosts() {
            alias_shared[value.index()] = true;
        }
        Ok(Self {
            builder: Some(builder),
            schema,
            graph,
            source,
            dialect,
            // A decoded source whose map is `None` IS the source's own bytes (UTF-8, BOM skip included): the graph's
            // decoded-coordinate spans are then source-relative by construction.
            source_mapped: decoded.maps_to_source(),
            spans_committed: false,
            memo: Vec::new(),
            in_progress: Vec::new(),
            equality,
            prune: prune.cloned(),
            alias_shared,
            want_tags,
        })
    }

    fn build_node(
        &mut self,
        node: GraphNode,
        prune: u32,
        resources: &mut ResourceContext<'_>,
    ) -> Result<NodeId, CodecError> {
        // A cyclic alias graph cannot become a semantic value: an alias re-entering a node still on the recursion path
        // is a cycle. This check runs BEFORE the memo, because container nodes are memoized before their children are
        // built — a memo-first lookup would let `&x [*x]` answer the sequence's own id and publish a self-referential
        // document. Completed nodes are popped from `in_progress` and still memo-hit, so DAG sharing is unaffected.
        if self.in_progress.contains(&node) {
            return Err(CodecError::new(CodecFailureKind::UnsupportedRepresentation));
        }
        // Memo: aliases reference the same document node.
        if let Some(id) = self.memo.get(node.index()).copied().flatten() {
            return Ok(id);
        }
        // An alias-shared node is built WHOLE at its first reach: the memo serves that build to every alias site, and
        // the tree is position-based (two alias sites may demand different members of the same node), so pruning the
        // first build could under-deliver a later site. Over-keeping is monotone-sound.
        let prune = if self.alias_shared.get(node.index()).copied().unwrap_or(false) {
            PRUNE_ALL
        } else {
            prune
        };
        self.in_progress.push(node);
        // The nesting guard: the walk recurses once per container level, so the accounted depth is the document's
        // structural depth. The guard's 10000-level ceiling makes the reference's ceiling message a clean codec error
        // instead of a Rust stack overflow at reduced stacks (the standing stack-depth gate's YAML lane).
        let _depth = resources.enter_nesting_owned().map_err(CodecError::from)?;
        let result = self.build_node_inner(node, prune, resources);
        self.in_progress.pop();
        result
    }

    fn build_node_inner(
        &mut self,
        node: GraphNode,
        prune: u32,
        resources: &mut ResourceContext<'_>,
    ) -> Result<NodeId, CodecError> {
        let yaml_node = self.graph.node_opt(node, self.source).ok_or_else(|| {
            CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "YAML graph node missing during document build",
            })
        })?;
        match yaml_node {
            YamlNode::Alias(target) => self.build_node(target, prune, resources),
            YamlNode::Scalar { .. } => self.build_scalar(node, resources),
            YamlNode::Sequence { .. } => self.build_sequence(node, prune, resources),
            YamlNode::Mapping { .. } => self.build_mapping(node, prune, resources),
        }
    }

    fn build_scalar(&mut self, node: GraphNode, resources: &mut ResourceContext<'_>) -> Result<NodeId, CodecError> {
        // The graph node's text source and style decide the span commit: a scalar whose authored bytes re-resolve to
        // the same value can name its source span instead of the arena, which is what lets the edit lane patch it
        // minimally.
        let YamlNode::Scalar { style, span, .. } = self.graph.node(node, self.source) else {
            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "YAML scalar walk over a non-scalar node",
            }));
        };
        let (style, node_span) = (style, span);
        let resolved = schema::resolve_scalar(self.graph, node, self.dialect, self.source)?;
        // Compute the payload BEFORE borrowing the builder (the scalar text borrows the graph and the source, both
        // disjoint from the builder). The text is BORROWED from the graph, never copied: a scalar whose span commits
        // names the source bytes and never reads the text again, and the stored-semantic path's `add_node` copies into
        // tracked storage itself — an owned local copy would be dropped unused in the dominant bound-source case.
        let text = self.scalar_text(node);
        // The float and integer payloads are computed ONLY for the category the schema resolved (a plain "hello" is a
        // string and must not fail a float parse). The canonical int text is hoisted because the Integer semantic
        // borrows it until `add_node` copies it; only an Integer scalar pays the canonicalization.
        let canonical: Option<String> = match &resolved {
            schema::ResolvedScalar::Core {
                category: ScalarCategory::Integer,
                ..
            } => canonical_integer(text),
            _ => None,
        };
        // The decode-unified float number is hoisted the same way, so its canonical decimal coefficient outlives the
        // builder call.
        let float_number: Option<ScalarNumber> = match &resolved {
            schema::ResolvedScalar::Core {
                category: ScalarCategory::Float,
                ..
            } => scalar_number_of(text),
            _ => None,
        };
        // The resolved tag text is COPIED into tracked storage by `add_node`; the local owns it for the call. A Core
        // tag is a static constant and a Tagged tag is already owned by the resolution, so borrow instead of allocating
        // a third copy.
        let resolved_tag: Option<&str> = match &resolved {
            schema::ResolvedScalar::Core { tag, .. } => Some(tag),
            schema::ResolvedScalar::Tagged { tag, .. } => Some(tag.as_str()),
        };
        // The bound-source span decision: a scalar whose authored bytes re-resolve to the same value names its source
        // span instead of the arena, which is what lets the edit lane patch it minimally. A PLAIN scalar qualifies by
        // the zero-copy law (decoded text == full token bytes); a QUOTED scalar qualifies only when its inner content
        // is escape-free (decoded text == raw inner bytes), the same law the JSON/TOML codecs keep for their
        // source-backed strings. A span may be committed only against the SOURCE's own bytes — the decoded text the
        // graph spans address must BE the source (UTF-8 input); a UTF-16/32 input's decoded buffer is not the source,
        // so no span is committed and an edit of such a document declines to the whole-document floor.
        let authored_span: Option<Span> = if self.source_mapped {
            match style {
                ScalarStyle::Plain => {
                    // The scanner's plain-scalar token extends past the value through its trailing separation AND any
                    // inline `# comment` — the decoded VALUE itself is trimmed of both, and the comment is consumed
                    // into the token's span (`port: 8080 # note` tokens as `8080 # note`). The authored content is the
                    // value's own bytes, a PREFIX of the token, so a span commits only when the token opens with the
                    // value and everything after it is the scanner's trailing material (separation, or a comment
                    // running to the line break — the token never extends past a break after a comment). Only a
                    // single-line scalar can qualify: a multi-line plain scalar folds its breaks, so its decoded text
                    // never byte-equals a source prefix.
                    let start = node_span.start() as usize;
                    let bytes = self.source.bytes().get(start..node_span.end() as usize);
                    let content_len = text.len();
                    let trailing_ok = match bytes {
                        Some(bytes) if bytes.starts_with(text.as_bytes()) => {
                            trailing_plain_material(&bytes[content_len..])
                        }
                        _ => false,
                    };
                    if trailing_ok {
                        Span::try_from_usize(start, start + content_len).ok()
                    } else {
                        None
                    }
                }
                ScalarStyle::SingleQuoted | ScalarStyle::DoubleQuoted => {
                    let inner = Span::try_from_usize(
                        node_span.start() as usize + 1,
                        (node_span.end() as usize).saturating_sub(1),
                    )
                    .ok();
                    match inner {
                        Some(inner)
                            if self.source.bytes().get(inner.start() as usize..inner.end() as usize)
                                == Some(text.as_bytes()) =>
                        {
                            Some(inner)
                        }
                        _ => None,
                    }
                }
                // A block scalar (literal `|` or folded `>`) never byte-equals its decoded text (block
                // folding/chomping), so it has no bound-source node kind. Its AUTHORED span is the whole block — the
                // `|`/`>` header through the last content line — which is the ruled splice policy: a block-scalar edit
                // replaces the whole scalar span, because a block's indentation and header are structure, not content.
                // The trailing line break is trimmed: the scanner's span includes the break after the last content
                // line, and the break is the line TERMINATOR, not the scalar — leaving it makes a spliced block scalar
                // keep its own newline (and never eat the break before the next member).
                ScalarStyle::Literal | ScalarStyle::Folded => {
                    let start = node_span.start() as usize;
                    let end = trim_trailing_break(self.source.bytes(), node_span.end() as usize);
                    Span::try_from_usize(start, end).ok()
                }
            }
        } else {
            None
        };
        // A canonical integer whose authored spelling byte-matches the canonical jqf rendering names its span; a
        // non-canonical spelling (hex, underscores, a leading `+`) does not — the same law TOML's span-eligible
        // integers keep, and what stops an edit from rewriting `0x10` to `16` when the program sets the same value.
        let (semantic, intrinsic, span_commit, authored_record) = match resolved {
            schema::ResolvedScalar::Core { category, .. } => {
                let (semantic, commit, authored) = match category {
                    ScalarCategory::String => {
                        // A block scalar's span names the whole `|`/`>` block, which never byte-equals the decoded
                        // text, so it is an OUT-OF-BAND authored span like a float's token — never a bound-source
                        // commit. Quoted and plain scalars keep the bound-source route.
                        let block = matches!(style, ScalarStyle::Literal | ScalarStyle::Folded);
                        let commit = if block { None } else { authored_span };
                        let authored = if block { authored_span } else { None };
                        (AccountedSemanticNode::String(text), commit, authored)
                    }
                    ScalarCategory::Null => (AccountedSemanticNode::Null, None, None),
                    ScalarCategory::Bool(value) => {
                        // A boolean's authored token (`true`/`false`) IS its canonical render, but it has no
                        // bound-source node kind; the authored span is recorded out-of-band so the edit lane can
                        // address and echo it verbatim.
                        (AccountedSemanticNode::Bool(value), None, authored_span)
                    }
                    ScalarCategory::Integer => {
                        // The canonical text is COPIED into the builder's tracked storage by `add_node`; the local only
                        // lives for the call.
                        let canonical = canonical.as_deref().ok_or_else(|| {
                            error::invalid_range(
                                self.source,
                                span_start(self.graph, node),
                                span_end(self.graph, node),
                                "int",
                                "invalid integer literal",
                            )
                        })?;
                        let commit = authored_span.filter(|_| canonical == text);
                        (AccountedSemanticNode::Integer(canonical), commit, None)
                    }
                    ScalarCategory::Float => {
                        // Only the FLOAT category reaches a number build (a string "hello" must not build-and-discard a
                        // numeric diagnostic on every scalar). Finite spellings unify to exact decimals; the binary64
                        // kind survives only for `.inf`/`-.inf`/`.nan`.
                        let semantic = match float_number.as_ref().ok_or_else(|| {
                            error::invalid_range(
                                self.source,
                                span_start(self.graph, node),
                                span_end(self.graph, node),
                                "float",
                                "invalid float literal",
                            )
                        })? {
                            ScalarNumber::Binary64(float) => AccountedSemanticNode::Float(*float),
                            ScalarNumber::Decimal(coefficient, scale) => AccountedSemanticNode::Decimal {
                                coefficient,
                                scale: *scale,
                            },
                        };
                        // A float's authored spelling (`1.50`) is NOT its canonical render (`1.5`), so it has no
                        // bound-source node kind; the authored span is recorded out-of-band — the semantic stays
                        // stored, and the span only addresses the authored token for the edit lane's verbatim echo and
                        // patching.
                        (semantic, None, authored_span)
                    }
                };
                let intrinsic = resolved_tag.map(|tag| AccountedIntrinsicTag::Core {
                    tag,
                    kind: ValueKind::from(category),
                });
                (semantic, intrinsic, commit, authored)
            }
            schema::ResolvedScalar::Tagged { payload, .. } => {
                let semantic = match payload {
                    ScalarCategory::String => AccountedSemanticNode::String(text),
                    _ => {
                        return Err(error::unsupported(
                            self.source,
                            span_start(self.graph, node),
                            span_end(self.graph, node),
                            "tag",
                            "a non-core tag around a non-string scalar is unrepresentable",
                        ));
                    }
                };
                let intrinsic = resolved_tag.map(AccountedIntrinsicTag::Tagged);
                (semantic, intrinsic, None, None)
            }
        };
        let intrinsic = demanded_intrinsic(self.want_tags, intrinsic);
        let span_commit = span_commit.filter(|_| self.source_mapped);
        let builder = self.builder.as_mut().expect("builder present");
        let id = if let Some(span) = span_commit {
            let scalar_kind = self.schema.node_kind(0).ok_or_else(data_contract)?;
            self.spans_committed = true;
            // SAFETY: the span names source bytes whose decoded text re-resolves
            // to the same semantic value (the zero-copy or escape-free byte-identity above, plus the integer
            // canonicality check), and admission proves containment against the seal the session binds before
            // publication.
            match &semantic {
                AccountedSemanticNode::String(_) => unsafe {
                    builder.add_prepared_bound_source_string_node(&self.schema, scalar_kind, span, resources)
                },
                AccountedSemanticNode::Integer(_) => unsafe {
                    builder.add_prepared_bound_source_integer_node(&self.schema, scalar_kind, span, resources)
                },
                _ => return Err(data_contract()),
            }
            .map_err(map_data)?
        } else {
            builder
                .add_node(SCALAR_KIND, semantic, intrinsic, resources)
                .map_err(map_data)?
        };
        // A scalar whose retained semantic carries no span of its own (a float, a boolean) records its authored token
        // out-of-band, so the edit lane can address it for verbatim echo and patching. The span is source-relative by
        // the same `source_mapped` law the bound spans keep; the bytes re-resolve to the stored semantic by
        // construction (the authored text was what parsed).
        if let Some(span) = authored_record.filter(|_| self.source_mapped) {
            // An out-of-band authored span is an admitted source span like any other: the seal binding is gated on
            // `spans_committed`, so recording one must set the flag or a document whose ONLY spans are out-of-band (a
            // root float/bool) would finalize without a seal covering them.
            self.spans_committed = true;
            // SAFETY: the span is this codec's own authored token over the
            // bound source, so it names UTF-8 that re-resolves to the stored semantic — the `record_authored_span`
            // contract.
            unsafe { builder.record_authored_span(id, span, resources) }.map_err(map_data)?;
        }
        self.record_memo(node, id);
        Ok(id)
    }

    fn build_sequence(
        &mut self,
        node: GraphNode,
        prune: u32,
        resources: &mut ResourceContext<'_>,
    ) -> Result<NodeId, CodecError> {
        let YamlNode::Sequence { items, tag, .. } = self.graph.node(node, self.source) else {
            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "YAML sequence walk over a non-sequence node",
            }));
        };
        let intrinsic = demanded_intrinsic(self.want_tags, tag.map(collection_intrinsic));
        let builder = self.builder.as_mut().expect("builder present");
        let id = builder
            .add_node(
                SEQ_KIND,
                AccountedSemanticNode::Array { item_role: ITEM_ROLE },
                intrinsic,
                resources,
            )
            .map_err(map_data)?;
        self.record_memo(node, id);
        // The container's authored span is recorded BEFORE its children are built (node ids strictly increase), so the
        // edit lane's structural append can name the collection's region and indentation.
        self.record_container_span(node, id, resources)?;
        let items: Vec<GraphNode> = items.to_vec();
        // Arrays never omit elements; each item's subtree prunes through the position's shared element node.
        let item_prune = PruneRef::root(self.prune.as_ref()).at(prune).element().id();
        for item in items {
            let child = self.build_node(item, item_prune, resources)?;
            let builder = self.builder.as_mut().expect("builder present");
            builder
                .add_occurrence(LocalOwnerRef::Node(id), ITEM_ROLE, None, child, resources)
                .map_err(map_data)?;
        }
        Ok(id)
    }

    fn build_mapping(
        &mut self,
        node: GraphNode,
        prune: u32,
        resources: &mut ResourceContext<'_>,
    ) -> Result<NodeId, CodecError> {
        let YamlNode::Mapping { entries, tag, .. } = self.graph.node(node, self.source) else {
            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "YAML mapping walk over a non-mapping node",
            }));
        };
        let intrinsic = demanded_intrinsic(self.want_tags, tag.map(collection_intrinsic));
        let builder = self.builder.as_mut().expect("builder present");
        let id = builder
            .add_node(
                MAP_KIND,
                AccountedSemanticNode::Object {
                    member_role: MEMBER_ROLE,
                },
                intrinsic,
                resources,
            )
            .map_err(map_data)?;
        self.record_memo(node, id);
        // The container's authored span is recorded BEFORE the two-phase child build (node ids strictly increase), so
        // the edit lane's structural append can name the collection's region and indentation.
        self.record_container_span(node, id, resources)?;
        let entries: Vec<(GraphNode, GraphNode)> = entries.to_vec();
        // TWO-PHASE build. Duplicate-key validation under the equivalence law runs in phase 1, where the accumulated
        // keys live in a LOCAL vec: a nested mapping's own build runs its own phase-1/2 walk and can never clobber this
        // mapping's accumulated keys. (The old shared `seen_keys` field was cleared by the nested walk, leaking the
        // nested mapping's keys into the parent's comparison and falsely rejecting `a: {b: 1}\nb: 2` as a duplicate
        // `b`.)
        //
        // The seen-text index is an exact SHORTLIST, never a substitute for the comparator: `keyed` holds only direct
        // core-string keys (a non-string key refuses in `key_text` before it can be pushed), and two such keys with the
        // same text are equal under the law (strings compare by Unicode scalar sequence), so an absent fingerprint
        // cannot be a duplicate. A fingerprint hit still runs the frozen comparator, which stays the decision-maker (a
        // collision only adds one comparison). The index turns a flat mapping's quadratic scan into a lookup; complex
        // keys never enter it.
        //
        // The buckets CHAIN their occupants: a single-slot map would let two different texts sharing a fingerprint
        // overwrite each other, hiding an exact duplicate behind the last occupant.
        let mut keyed: Vec<(GraphNode, GraphNode, String)> = Vec::new();
        let mut seen_text: BTreeMap<u64, Vec<GraphNode>> = BTreeMap::new();
        for (key_node, value_node) in &entries {
            // A member the prune hint names unobservable still has its KEY VALIDATED (the yaml.key-equivalence@1
            // duplicate-key law runs before projection, and this loop runs it for EVERY key), but its key's document
            // node is never built — phase 2 omits the member, so the node would be dead arena, and on a count-shaped
            // spine (the yaml-count-gate) the pruned elements' keys are the document's bulk. The key text is graph-only
            // (see [`Self::key_text`]).
            let key_text = self.key_text(*key_node)?;
            let omitted = PruneRef::root(self.prune.as_ref())
                .at(prune)
                .member(key_text.as_bytes())
                .is_none();
            if !omitted {
                self.build_node(*key_node, PRUNE_ALL, resources)?;
            }
            if let Some(occupants) = seen_text.get(&fingerprint(&key_text)) {
                for &previous in occupants {
                    let verdict = self.equality.equals(previous, *key_node, resources)?;
                    if verdict == Verdict::Equal {
                        return Err(error::invalid_range(
                            self.source,
                            span_start(self.graph, *key_node),
                            span_end(self.graph, *key_node),
                            "duplicate-key",
                            "mapping key is a duplicate under yaml.key-equivalence@1",
                        ));
                    }
                }
            }
            seen_text.entry(fingerprint(&key_text)).or_default().push(*key_node);
            if !omitted {
                keyed.push((*key_node, *value_node, key_text));
            }
        }
        // Phase 2: build every kept value (keys are already built and memoized) and add the occurrences in entry order.
        // A member the prune hint names unobservable is OMITTED — its key was still validated in phase 1, but its value
        // is never read by the program.
        for (_key_node, value_node, key_text) in keyed {
            let Some(value_prune) = PruneRef::root(self.prune.as_ref())
                .at(prune)
                .member(key_text.as_bytes())
            else {
                continue;
            };
            let value_id = self.build_node(value_node, value_prune, resources)?;
            let builder = self.builder.as_mut().expect("builder present");
            builder
                .add_occurrence(
                    LocalOwnerRef::Node(id),
                    MEMBER_ROLE,
                    Some(AccountedOccurrenceKey::Text(&key_text)),
                    value_id,
                    resources,
                )
                .map_err(map_data)?;
        }
        Ok(id)
    }

    /// The object-key text of a mapping key: the key must be a direct core string (quoted, explicit `!!str`, or a plain
    /// scalar that resolves to String under the schema). A complex or non-core-tagged key is never coerced: it makes
    /// the mapping unrepresentable in the semantic document (the graph retains it).
    fn key_text(&mut self, key_node: GraphNode) -> Result<String, CodecError> {
        let node = self.graph.node(key_node, self.source);
        let is_string = match node {
            YamlNode::Scalar { text, tag, style, .. } => {
                let quoted = style != crate::graph::ScalarStyle::Plain;
                let explicit_str = tag == Some(TAG_STR);
                // An empty plain scalar used as a MAPPING KEY is the empty string (`: a` — the corpus's empty-key
                // reading), even though as a VALUE the core schema resolves it to null.
                let empty_key_str = !quoted && !explicit_str && text.is_empty();
                let resolved_str = !quoted
                    && !explicit_str
                    && matches!(
                        schema::resolve_scalar(self.graph, key_node, self.dialect, self.source,)?,
                        schema::ResolvedScalar::Core {
                            category: ScalarCategory::String,
                            ..
                        }
                    );
                quoted || explicit_str || empty_key_str || resolved_str
            }
            _ => false,
        };
        if !is_string {
            let message = match node {
                YamlNode::Mapping { .. } | YamlNode::Sequence { .. } => {
                    "a complex mapping key is not coerced to an object key"
                }
                YamlNode::Scalar { .. } => "a non-string mapping key is not coerced to an object key",
                YamlNode::Alias(_) => "an aliased mapping key is not coerced to an object key",
            };
            return Err(error::unsupported(
                self.source,
                span_start(self.graph, key_node),
                span_end(self.graph, key_node),
                "key",
                message,
            ));
        }
        Ok(self.scalar_text(key_node).to_owned())
    }

    /// The scalar's decoded text, borrowed from the graph for the WALK's lifetime (`'graph`), never from `&self` — a
    /// caller may hold it across a builder borrow, which is what lets `build_scalar` avoid an owned copy for
    /// span-committed scalars.
    fn scalar_text(&self, node: GraphNode) -> &'graph str {
        match self.graph.node(node, self.source) {
            YamlNode::Scalar { text, .. } => text,
            _ => "",
        }
    }

    fn record_memo(&mut self, node: GraphNode, id: NodeId) {
        let index = node.index();
        while self.memo.len() <= index {
            self.memo.push(None);
        }
        self.memo[index] = Some(id);
    }

    /// Records the container's out-of-band authored span: a FLOW collection's own graph region (its closing delimiter
    /// makes the flow splice detectable), or a BLOCK collection's span from its first entry's start to its last entry's
    /// subtree end — the region the edit lane's structural append closes on. Recorded before the children build, so the
    /// strictly-increasing node-id order `record_authored_span` demands holds. A span may be committed only against the
    /// SOURCE's own bytes (UTF-8 input), the same `source_mapped` gate the scalars keep.
    fn record_container_span(
        &mut self,
        node: GraphNode,
        id: NodeId,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        if !self.source_mapped {
            return Ok(());
        }
        let span = container_authored_span(self.graph, node, self.source);
        let Some(span) = span else {
            return Ok(());
        };
        self.spans_committed = true;
        // SAFETY: the span names source bytes that re-resolve to this
        // container's value (the collection's authored text is what parsed), and admission proves containment against
        // the seal the session binds before publication.
        unsafe {
            self.builder
                .as_mut()
                .expect("builder present")
                .record_authored_span(id, span, resources)
        }
        .map_err(map_data)
    }
}

/// Attaches one `yaml.comment@1` fact per node whose leading comments the parser recorded. A comment's owner is the
/// node whose source span begins at or after the comment's end — the cross-format leading-comment model (a trailing
/// comment after a value becomes the next node's leading comment; a document-trailer comment with NO following node
/// pins to the document ROOT, the cross-format detached-comment owner). A mapping-key owner redirects to its VALUE's
/// document node, because `.key` resolves there. Shared by the whole-document and scoped routes so `.key.@comment`
/// serves regardless of which route the query takes. Graph-derived data the comment-association pass needs, computed
/// ONCE per graph. Without this hoist, every scoped build re-walked the whole node arena, re-sorted the spans, and
/// rebuilt the key→value redirect map — work that depends only on the graph, never on the located subtree.
pub(crate) struct CommentIndex {
    /// Every recorded comment span, in source order.
    comments: Vec<Span>,
    /// Non-zero-width node spans sorted by `(start, end)`: a node whose span STARTS at the same offset as a container
    /// (a block mapping opens at its first key) must lose to the more specific node — the key itself — so the leading
    /// comment lands on the member, not the container. Zero-width spans (a block collection's start token occupies no
    /// content bytes) are excluded outright: a token boundary cannot own a comment.
    spans: Vec<(GraphNode, u32, u32)>,
    /// Mapping key node → value node, for the comment-to-value redirect (a leading comment before a key must attach to
    /// the key's VALUE's document node, because `.key` resolves there).
    key_to_value: BTreeMap<usize, GraphNode>,
    /// One parent COLLECTION per node, indexed by graph node index: the foot-owner walk climbs this chain to find the
    /// deepest ancestor collection at or shallower than the closing comment's own column. The root has no parent.
    parent: Vec<Option<GraphNode>>,
    /// Every node's authored span INCLUDING the zero-width collection opens (a block collection's span is its first
    /// key's / dash offset), indexed by graph node index: the column lookup for the foot rule.
    node_span: Vec<(u32, u32)>,
    /// The document root (the trailer-comment owner).
    root: Option<GraphNode>,
}

impl CommentIndex {
    pub(crate) fn from_graph(graph: &YamlGraph) -> Self {
        let all = graph.node_span_pairs();
        let mut node_span = alloc::vec![(0u32, 0u32); graph.len()];
        for (node, start, end) in &all {
            node_span[node.index()] = (*start, *end);
        }
        let mut spans = all;
        spans.retain(|(_, start, end)| start != end);
        spans.sort_by_key(|(_, start, end)| (*start, *end));
        Self {
            comments: graph.comments().to_vec(),
            spans,
            key_to_value: graph
                .key_value_pairs()
                .into_iter()
                .map(|(key, value)| (key.index(), value))
                .collect(),
            parent: graph.parent_collections(),
            node_span,
            root: graph.root(),
        }
    }
}

/// Attaches the three YAML comment facts per node whose comments the parser recorded: `yaml.comment@1` (leading),
/// `yaml.comment_inline@1` (same-line), and `yaml.comment_foot@1` (the closing block's foot). The classification is
/// pure span arithmetic over the graph's displaced node spans:
///
/// - **Inline.** A comment whose bytes from the PRECEDING node span's end to its own start contain no line break sits
///   on that node's line and belongs to it. `a: 1 # note` answers under `.a.@comment_inline`, never under the next
///   node's leading list.
/// - **Foot.** A comment on its own line is a foot of the block that is closing when it is indented DEEPER than the
///   NEXT node's column; otherwise it is the next node's leading comment. The owner is the deepest ancestor collection
///   of the preceding node at or shallower than the comment's own column. A document-trailer comment (no following
///   node) keeps its ROOT owner and is re-labelled as the root's foot.
///
/// A mapping-key owner redirects to its VALUE's document node, because `.key` resolves there. Shared by the
/// whole-document and scoped routes so `.key.@comment` serves regardless of which route the query takes. `None` means
/// the graph carries no comments.
pub(crate) fn attach_comment_facts(
    builder: &mut AccountedDocumentBuilder<'static>,
    index: Option<&CommentIndex>,
    memo: &[Option<NodeId>],
    source: ResolvedSource<'_>,
    source_mapped: bool,
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    // A comment span is a GRAPH span: decoded-text coordinates. Slicing the source's own bytes with it is only sound
    // when the decoded text IS the source (UTF-8 input, the walker's `source_mapped` law). A UTF-16/32 input's decoded
    // buffer is longer than the source, so a decoded-coordinate span can overshoot `source.bytes()` and panic. The
    // walker declines every other source-committed span under the same law, so comments decline with them (the document
    // still builds; only the source-slice comment facts are absent).
    if !source_mapped {
        return Ok(());
    }
    let Some(index) = index else {
        return Ok(());
    };
    let bytes = source.bytes();
    // comment spans -> owning document node -> comment texts, one map per position. Iterating comments in source order
    // keeps each list in source order.
    let mut leading: alloc::collections::BTreeMap<u64, Vec<String>> = alloc::collections::BTreeMap::new();
    let mut inline: alloc::collections::BTreeMap<u64, Vec<String>> = alloc::collections::BTreeMap::new();
    let mut foot: alloc::collections::BTreeMap<u64, Vec<String>> = alloc::collections::BTreeMap::new();
    for comment in &index.comments {
        let start = comment.start();
        let end = comment.end();
        // The spans are sorted by `(start, end)`, so the first span whose start is at or after the comment's end sits
        // at the binary-search split — a linear `find` per run was wasted work.
        let next_idx = index.spans.partition_point(|(_, s, _)| *s < end);
        // The PRECEDING node: the last span whose END is at or before the comment's start — `partition_point` in the
        // other direction. The non-zero spans are disjoint source runs, so their ends are sorted too and the predicate
        // is monotonic.
        let prev_idx = index.spans.partition_point(|(_, _, e)| *e <= start);
        let prev = match prev_idx {
            0 => None,
            i => Some(index.spans[i - 1]),
        };
        let next = index.spans.get(next_idx).copied();
        // 1. Inline: the preceding node's own line carries the comment.
        if let Some((prev_node, _, prev_end)) = prev {
            let between = &bytes[prev_end as usize..start as usize];
            if !between.contains(&b'\n') {
                if let Some(doc_owner) = doc_owner(index, memo, prev_node) {
                    let text = comment_text(bytes, *comment);
                    inline.entry(doc_owner.get()).or_default().push(text);
                }
                continue;
            }
        }
        // 2. The document trailer: no node follows, so it pins to the DOCUMENT ROOT and is the root's FOOT — the
        //    cross-format detached-comment model, re-labelled from leading.
        let Some((next_node, next_start, _)) = next else {
            if let Some(root) = index.root
                && let Some(doc_root) = memo.get(root.index()).copied().flatten()
            {
                let text = comment_text(bytes, *comment);
                foot.entry(doc_root.get()).or_default().push(text);
            }
            continue;
        };
        // 3. A comment on its own line with a following node: the column rule decides foot versus the next node's
        //    leading.
        let comment_col = byte_column(bytes, start);
        let next_col = byte_column(bytes, next_start);
        let owner = if comment_col > next_col {
            // A comment indented deeper than the next node closes a block; its owner is the deepest ancestor collection
            // of the preceding node at or shallower than the comment's own column. With a preceding node the chain
            // always names one (the root is the last resort); with none, a leading comment is the only defensible
            // answer.
            prev.and_then(|(prev_node, _, _)| foot_owner(index, bytes, prev_node, comment_col))
                .unwrap_or(next_node)
        } else {
            next_node
        };
        if let Some(doc_owner) = doc_owner(index, memo, owner) {
            let text = comment_text(bytes, *comment);
            if owner == next_node {
                leading.entry(doc_owner.get()).or_default().push(text);
            } else {
                foot.entry(doc_owner.get()).or_default().push(text);
            }
        }
    }
    for (map, fact) in [
        (&leading, COMMENT_FACT),
        (&inline, COMMENT_INLINE_FACT),
        (&foot, COMMENT_FOOT_FACT),
    ] {
        for (node, texts) in map {
            let payload = FactPayload::List(
                texts
                    .iter()
                    .cloned()
                    .map(FactPayload::Text)
                    .collect::<alloc::vec::Vec<_>>(),
            );
            let Ok(node) = usize::try_from(*node) else {
                continue;
            };
            let Some(node) = jqf_data::NodeId::try_from_index(node) else {
                continue;
            };
            builder
                .add_fact(LocalOwnerRef::Node(node), fact, fact, 1, &payload, resources)
                .map_err(map_data)?;
        }
    }
    Ok(())
}

/// Resolves a graph node's owning document node: a mapping-key owner redirects to its VALUE's document node (`.key`
/// resolves there); anything outside the built subtree (the scoped route's memo) attaches nothing.
fn doc_owner(index: &CommentIndex, memo: &[Option<NodeId>], owner: GraphNode) -> Option<NodeId> {
    index
        .key_to_value
        .get(&owner.index())
        .copied()
        .and_then(|value| memo.get(value.index()).copied().flatten())
        .or_else(|| memo.get(owner.index()).copied().flatten())
}

/// The zero-based column of an offset: bytes since the last line break.
fn byte_column(bytes: &[u8], offset: u32) -> usize {
    let offset = offset as usize;
    match bytes[..offset].iter().rposition(|&b| b == b'\n') {
        Some(line_start) => offset - line_start - 1,
        None => offset,
    }
}

/// The closing block's owner for a foot comment: climb the preceding node's parent chain and return the first (deepest)
/// collection at or shallower than the comment's own column. YAML indentation makes columns strictly shallower going
/// up, so the first match is the deepest one.
fn foot_owner(index: &CommentIndex, bytes: &[u8], prev: GraphNode, comment_col: usize) -> Option<GraphNode> {
    let mut current = prev;
    loop {
        let parent = index.parent.get(current.index()).copied().flatten()?;
        let (parent_start, _) = index.node_span.get(parent.index()).copied()?;
        if byte_column(bytes, parent_start) <= comment_col {
            return Some(parent);
        }
        current = parent;
    }
}

/// Attaches one `yaml.style@1` fact (payload = the authored scalar style name) to every scalar document node. The style
/// is read from the node's own graph record, so a scalar's fact names exactly how its authored content was spelled; the
/// write half re-renders the span to a requested style and the edit lane's verify compares this re-decoded fact to the
/// payload.
pub(crate) fn attach_style_facts(
    builder: &mut AccountedDocumentBuilder<'static>,
    graph: &YamlGraph,
    memo: &[Option<NodeId>],
    source: ResolvedSource<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    for index in 0..graph.len() {
        let Some(doc_node) = memo.get(index).copied().flatten() else {
            continue;
        };
        let Some(YamlNode::Scalar { style, .. }) =
            graph.node_opt(GraphNode(u32::try_from(index).unwrap_or(u32::MAX)), source)
        else {
            continue;
        };
        let name = scalar_style_name(style);
        builder
            .add_fact(
                LocalOwnerRef::Node(doc_node),
                STYLE_FACT,
                STYLE_FACT,
                1,
                &FactPayload::Text(name.into()),
                resources,
            )
            .map_err(map_data)?;
    }
    Ok(())
}

/// The [`ScalarStyle`] name as the fact payload spells it.
pub(crate) fn scalar_style_name(style: ScalarStyle) -> &'static str {
    match style {
        ScalarStyle::Plain => "plain",
        ScalarStyle::SingleQuoted => "single",
        ScalarStyle::DoubleQuoted => "double",
        ScalarStyle::Literal => "literal",
        ScalarStyle::Folded => "folded",
    }
}

/// Attaches one `yaml.anchor@1` fact to every anchored document node (the block encoder's anchor/alias emission). The
/// name is read from the node's own graph properties — the parser stores the `&name` it bound — so a node with no
/// authored anchor gets no fact, whatever sharing the document walk created. The memo names each graph node's document
/// node; an alias-shared node is anchored exactly once, at the node's own position.
pub(crate) fn attach_anchor_facts(
    builder: &mut AccountedDocumentBuilder<'static>,
    graph: &YamlGraph,
    memo: &[Option<NodeId>],
    source: ResolvedSource<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    for index in 0..graph.len() {
        let Some(doc_node) = memo.get(index).copied().flatten() else {
            continue;
        };
        let Some(anchor) = graph_anchor_name(graph, GraphNode(u32::try_from(index).unwrap_or(u32::MAX)), source) else {
            continue;
        };
        builder
            .add_fact(
                LocalOwnerRef::Node(doc_node),
                ANCHOR_FACT,
                ANCHOR_FACT,
                1,
                &FactPayload::Text(anchor.into()),
                resources,
            )
            .map_err(map_data)?;
    }
    Ok(())
}

/// The authored anchor name of one graph node, from its packed properties. An alias occurrence is a reference and
/// carries no anchor of its own.
fn graph_anchor_name<'a>(graph: &'a YamlGraph, node: GraphNode, source: ResolvedSource<'a>) -> Option<&'a str> {
    match graph.node_opt(node, source)? {
        YamlNode::Scalar { anchor, .. } | YamlNode::Sequence { anchor, .. } | YamlNode::Mapping { anchor, .. } => {
            anchor
        }
        YamlNode::Alias(_) => None,
    }
}

/// Attaches one `edit-refusal` fact per alias-REFERENCED node: the walk shares ONE document node across an anchor and
/// every alias that references it, so a value write through either path would patch the anchor's authored span and
/// silently change every other alias site. The fact's role is the format-neutral [`jqf_codec_core::EDIT_REFUSAL_ROLE`],
/// its payload the prose refusal; the edit lane reads the role by identity and raises the message instead of patching.
///
/// The refusal covers the DESCENDANTS of each target too: a merge key (`<<: *anchor`) splices the anchored mapping's
/// entries into the host mapping by REUSING the source node ids, so `.svc_a.timeout` and `.defaults.timeout` are one
/// document node that is a descendant of the anchor, not the anchor itself — an edit to it would patch the anchor's
/// authored span. The cost is O(nodes in the aliased subtree) facts instead of O(alias targets); a document with one
/// large anchor pays the subtree once, which is the ledger charge the honesty buys.
pub(crate) fn attach_alias_refusal_facts(
    builder: &mut AccountedDocumentBuilder<'static>,
    graph: &YamlGraph,
    memo: &[Option<NodeId>],
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    for target in graph.alias_targets() {
        for node in graph.subtree_nodes(target) {
            let Some(doc_node) = memo.get(node.index()).copied().flatten() else {
                continue;
            };
            builder
                .add_fact(
                    LocalOwnerRef::Node(doc_node),
                    jqf_codec_core::EDIT_REFUSAL_ROLE,
                    jqf_codec_core::EDIT_REFUSAL_ROLE,
                    1,
                    &FactPayload::Text(ALIAS_REFUSAL_MESSAGE.into()),
                    resources,
                )
                .map_err(map_data)?;
        }
    }
    Ok(())
}

/// Attaches one `merge-override` fact per MERGE-INHERITED member: the merged entry reuses the anchored mapping's node
/// ids, so the document cannot tell `.svc_a.timeout` — a member `<<:` spliced in from `&defaults` — from a host member
/// by the node alone. The fact's role is the format-neutral [`jqf_codec_core::MERGE_OVERRIDE_ROLE`]; its payload is the
/// HOST mapping's document node id, the container a write to the member must splice into. The edit lane reads the role
/// by identity and compares the payload against the container it is diffing: a write that reaches the member THROUGH
/// its host becomes a local override (the whole new member splices into the host), while a write through the ANCHOR
/// itself sees a different container and stays under the alias refusal. The cost is O(admitted merged entries), not
/// O(nodes in the anchor subtree — the refusal facts still own that charge.
pub(crate) fn attach_merge_override_facts(
    builder: &mut AccountedDocumentBuilder<'static>,
    graph: &YamlGraph,
    memo: &[Option<NodeId>],
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    for (value, host) in graph.merge_hosts() {
        let Some(value_doc) = memo.get(value.index()).copied().flatten() else {
            continue;
        };
        let Some(host_doc) = memo.get(host.index()).copied().flatten() else {
            continue;
        };
        builder
            .add_fact(
                LocalOwnerRef::Node(value_doc),
                jqf_codec_core::MERGE_OVERRIDE_ROLE,
                jqf_codec_core::MERGE_OVERRIDE_ROLE,
                1,
                &FactPayload::Integer(jqf_data::Integer::from_i64(
                    i64::try_from(host_doc.get()).unwrap_or(i64::MAX),
                )),
                resources,
            )
            .map_err(map_data)?;
    }
    Ok(())
}

/// The user-facing comment text: the source bytes after the leading `#`. Extraction removes the delimiter and line
/// terminator, then exactly ONE immediately following ASCII space when present; every remaining scalar (further spaces,
/// tabs) is text. The scanner's span covers `# ...` up to the line break, so the terminator strip is defensive.
fn comment_text(source: &[u8], span: jqf_source::Span) -> String {
    let start = span.start() as usize;
    let end = span.end() as usize;
    let text = core::str::from_utf8(&source[start..end]).unwrap_or("");
    let text = text.strip_prefix('#').unwrap_or(text);
    let text = text.strip_suffix(['\n', '\r']).unwrap_or(text);
    match text.strip_prefix(' ') {
        Some(rest) => rest.to_owned(),
        None => text.to_owned(),
    }
}

fn span_start(graph: &YamlGraph, node: GraphNode) -> usize {
    graph.node_span(node).start() as usize
}

fn span_end(graph: &YamlGraph, node: GraphNode) -> usize {
    graph.node_span(node).end() as usize
}

/// The authored source region of one collection node: a FLOW collection's own graph span (the `{`/`[` and its closing
/// delimiter bound the flow splice), or a BLOCK collection's span from its FIRST entry's start to its LAST entry's
/// whole-subtree end, with the trailing line break trimmed (the scanner's plain-scalar token carries its line's break;
/// the break is the line TERMINATOR, not the container). `None` for a non-collection.
#[allow(
    clippy::cast_possible_truncation,
    reason = "the graph's span coordinates are u32 by construction; a source beyond 4 GiB cannot parse"
)]
fn container_authored_span(graph: &YamlGraph, node: GraphNode, source: ResolvedSource<'_>) -> Option<Span> {
    let yaml_node = graph.node_opt(node, source)?;
    match yaml_node {
        YamlNode::Mapping { entries, span, .. } => {
            if span.end() > 0
                && source
                    .bytes()
                    .get(span.end() as usize - 1)
                    .is_some_and(|byte| *byte == b'{')
            {
                return Some(span);
            }
            let start = entries
                .first()
                .map_or(span.start(), |(key, _)| graph.node_span(*key).start());
            let end = entries.last().map_or(span.end(), |(_, value)| {
                trim_trailing_break(source.bytes(), graph.subtree_end(*value) as usize) as u32
            });
            Span::try_from_usize(start as usize, end as usize).ok()
        }
        YamlNode::Sequence { items, span, .. } => {
            if span.end() > 0
                && source
                    .bytes()
                    .get(span.end() as usize - 1)
                    .is_some_and(|byte| *byte == b'[')
            {
                return Some(span);
            }
            let start = items
                .first()
                .map_or(span.start(), |item| graph.node_span(*item).start());
            let end = items.last().map_or(span.end(), |item| {
                trim_trailing_break(source.bytes(), graph.subtree_end(*item) as usize) as u32
            });
            Span::try_from_usize(start as usize, end as usize).ok()
        }
        _ => None,
    }
}

/// Core tags are `.@tag` observations; skip them unless the program reads tags or preserves facts for re-encode. A
/// non-core Tagged tag *is* the value (`!money`), so identity still attaches it.
pub(crate) fn demanded_intrinsic(
    want_tags: bool,
    intrinsic: Option<AccountedIntrinsicTag<'_>>,
) -> Option<AccountedIntrinsicTag<'_>> {
    intrinsic.filter(|tag| want_tags || matches!(tag, AccountedIntrinsicTag::Tagged(_)))
}

/// The intrinsic tag for a collection tag: the resolved standard map/seq tags are core; anything else is a non-core tag
/// around the collection.
pub(crate) fn collection_intrinsic(tag: &str) -> AccountedIntrinsicTag<'_> {
    match tag {
        TAG_MAP => AccountedIntrinsicTag::Core {
            tag,
            kind: ValueKind::Object,
        },
        TAG_SEQ => AccountedIntrinsicTag::Core {
            tag,
            kind: ValueKind::Array,
        },
        _ => AccountedIntrinsicTag::Tagged(tag),
    }
}

impl From<ScalarCategory> for ValueKind {
    fn from(category: ScalarCategory) -> Self {
        match category {
            ScalarCategory::String => Self::String,
            ScalarCategory::Null => Self::Null,
            ScalarCategory::Bool(_) => Self::Bool,
            ScalarCategory::Integer | ScalarCategory::Float => Self::Number,
        }
    }
}

/// Radix spellings (`0x`/`0o`/`0b`) are canonicalized by schoolbook multiply-by-radix, which is quadratic in the digit
/// count: a radix literal past this bound is refused at decode rather than burning quadratic CPU. The DECIMAL spelling
/// of the same magnitude stays unbounded — this is a conversion-cost narrowing, not a magnitude law, and it uses the
/// same decode-refusal class as the decimal scale-out-of-range refusal.
const MAX_RADIX_DIGITS: usize = 8192;

/// Canonicalizes a YAML int spelling to jqf's canonical signed decimal text.
pub(crate) fn canonical_integer_for(text: &str) -> Option<String> {
    canonical_integer(text)
}

/// The decode-unified number of a YAML float spelling: a finite decimal/exponent spelling becomes an EXACT decimal, and
/// only the `.inf`/`-.inf`/`.nan` spellings keep the binary64 kind.
#[derive(Debug)]
pub(crate) enum ScalarNumber {
    /// A binary64 value (`.inf`/`-.inf`/`.nan`, per §4.8).
    Binary64(jqf_data::Float),
    /// A canonical exact decimal: signed coefficient text and scale.
    Decimal(alloc::string::String, i64),
}

/// Classifies a resolved float spelling into its decode-unified number, or returns `None` for a spelling the float
/// category resolved but that is not a valid decimal.
pub(crate) fn scalar_number_of(text: &str) -> Option<ScalarNumber> {
    if schema::is_nan_spelling(text) {
        return Some(ScalarNumber::Binary64(jqf_data::Float::new(f64::from_bits(
            schema::POSITIVE_QUIET_NAN_BITS,
        ))));
    }
    if schema::is_infinity_spelling(text) {
        return Some(ScalarNumber::Binary64(jqf_data::Float::new(
            if schema::is_negative_infinity(text) {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            },
        )));
    }
    let decimal = jqf_data::Decimal::parse(&decode_float_spelling(text)).ok()?;
    Some(ScalarNumber::Decimal(
        decimal.coefficient().as_str().to_owned(),
        decimal.scale(),
    ))
}

/// The FNV-1a fingerprint of an object-key text (the same mix law `jqf-codec-core`'s demand hashing uses). The
/// shortlist is exact because equal texts always share a fingerprint and a hit is re-decided by the frozen comparator;
/// a collision costs one extra comparison, never a wrong answer.
pub(crate) fn fingerprint(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// The exact decimal of a core float spelling: any leftover underscore is stripped before the decimal parse, which does
/// not accept them. The core float production itself has no underscores.
pub(crate) fn decode_float_spelling(text: &str) -> alloc::string::String {
    let mut out = alloc::string::String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte != b'_' {
            out.push(byte as char);
        }
    }
    out
}

/// Canonicalizes an ALREADY-VALIDATED core integer spelling (every caller reaches here through the Integer category
/// [`crate::schema`] resolves, i.e. `parse_yaml_int`) to jqf's canonical signed decimal text. The accepted vocabulary
/// is exactly the schema's: `[-+]?[0-9]+`, `0x[0-9a-fA-F]+`, and `0o[0-7]+` — no binary prefix, no uppercase radix
/// prefixes, no underscores, no sign on a radix form. Keeping this helper total over the same vocabulary prevents a
/// second, wider number law from growing beside the schema's.
fn canonical_integer(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    // A radix form is unsigned with a lowercase prefix; anything else falls through to the sign-and-decimal path, which
    // rejects it.
    if bytes.len() > 2 && bytes[0] == b'0' {
        match bytes[1] {
            b'x' => return radix_magnitude(&text[2..], 16),
            b'o' => return radix_magnitude(&text[2..], 8),
            _ => {}
        }
    }
    let (negative, unsigned) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => match text.strip_prefix('+') {
            Some(rest) => (false, rest),
            None => (false, text),
        },
    };
    if unsigned.is_empty() || !unsigned.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let trimmed = unsigned.trim_start_matches('0');
    let mut out = String::new();
    if negative && !trimmed.is_empty() {
        out.push('-');
    }
    out.push_str(if trimmed.is_empty() { "0" } else { trimmed });
    Some(out)
}

/// The canonical decimal magnitude of one radix form's digits: refused for an empty body or any digit illegal for the
/// radix (underscores included — the schema has none), and bounded at [`MAX_RADIX_DIGITS`] significant digits because
/// the conversion below is quadratic in that count.
fn radix_magnitude(digits: &str, radix: u32) -> Option<String> {
    if digits.is_empty() || !digits.bytes().all(|b| is_radix_digit(b, radix)) {
        return None;
    }
    let trimmed = digits.trim_start_matches('0');
    if trimmed.len() > MAX_RADIX_DIGITS {
        return None;
    }
    Some(radix_to_decimal(if trimmed.is_empty() { "0" } else { trimmed }, radix))
}

/// Accumulates validated radix digits into canonical decimal text by repeated multiply-by-radix: schoolbook arithmetic
/// over little-endian decimal digits (position 0 is the units). No u128 ceiling, matching the decimal spelling path's
/// exact-magnitude law; the caller has already validated every digit via [`is_radix_digit`] and bounded the significant
/// count to [`MAX_RADIX_DIGITS`]. Infallible.
fn radix_to_decimal(digits: &str, radix: u32) -> String {
    let mut acc: Vec<u8> = Vec::new();
    acc.push(0);
    for byte in digits.bytes() {
        let mut carry = u64::from(radix_digit_value(byte));
        for place in &mut acc {
            let total = u64::from(*place) * u64::from(radix) + carry;
            *place = (total % 10) as u8;
            carry = total / 10;
        }
        while carry > 0 {
            acc.push((carry % 10) as u8);
            carry /= 10;
        }
    }
    while acc.len() > 1 && *acc.last().expect("acc is never empty") == 0 {
        acc.pop();
    }
    let mut out = String::new();
    for &digit in acc.iter().rev() {
        out.push(char::from(b'0' + digit));
    }
    out
}

/// The numeric value of a radix digit byte. The caller has already checked [`is_radix_digit`], so the byte is a valid
/// digit for the radix.
fn radix_digit_value(byte: u8) -> u8 {
    if byte.is_ascii_digit() {
        byte - b'0'
    } else {
        byte.to_ascii_lowercase() - b'a' + 10
    }
}

fn is_radix_digit(byte: u8, radix: u32) -> bool {
    match byte {
        b'0'..=b'9' => u32::from(byte - b'0') < radix,
        b'a'..=b'f' | b'A'..=b'F' => radix == 16,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{COMMENT_FACT, COMMENT_FOOT_FACT, COMMENT_INLINE_FACT};
    use super::{MAX_RADIX_DIGITS, canonical_integer_for, comment_text};

    /// The codec's comment role constants are the shared vocabulary's spellings: the builders and the `'static`
    /// literals cannot drift apart.
    #[test]
    fn comment_roles_agree_with_the_shared_vocabulary() {
        use jqf_codec_core::comment;
        assert_eq!(COMMENT_FACT, comment::comment_role("yaml"));
        assert_eq!(COMMENT_INLINE_FACT, alloc::format!("yaml.{}@1", comment::INLINE));
        assert_eq!(COMMENT_FOOT_FACT, alloc::format!("yaml.{}@1", comment::FOOT));
    }

    use jqf_codec_core::{
        AccessFootprint, AccessGuarantees, AccessOutcome, AccessRequirement, CodecDemand, CodecError, CodecFailureKind,
        DemandClause, DiagnosticPolicy, ErasedProvider, ExactPath, ExactSelectionRecord, PruneTree, ValidationMode,
    };
    use jqf_data::{CountDemand, CountRow, CountStep, CountVerdict, FactKindId, FactPayloadView, FactRoleId, Value};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
    use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

    fn resources<'a>() -> ResourceContext<'a> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("context")
    }

    fn source(bytes: &[u8]) -> ResolvedSource<'_> {
        ResolvedSource::new(
            SourceRef::new(SourceId::new(1), SourceKind::Input),
            "test.yaml",
            bytes,
            0,
        )
    }

    fn simple_request() -> jqf_codec_core::DecodeRequest<'static> {
        let dialect: &'static jqf_data::DialectId = alloc::boxed::Box::leak(alloc::boxed::Box::new(
            jqf_data::DialectId::try_new(crate::YAML_CORE_DIALECT_ID).expect("dialect"),
        ));
        jqf_codec_core::DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect,
            options: None,
            allow_adjacent_values: false,
            value_separator: &[],
        }
    }

    /// Decodes one YAML document through the whole-document route and materializes it.
    fn decode(bytes: &'static [u8]) -> Result<Value, CodecError> {
        let mut resources = resources();
        let product = decode_product(bytes, &mut resources)?;
        product.document().materialize_root(&mut resources).map_err(|_error| {
            CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "materialize root",
            })
        })
    }

    /// Decodes one YAML document and returns its authoritative product, so span and attached-fact laws can be asserted.
    fn decode_product<'bytes>(
        bytes: &'bytes [u8],
        resources: &mut ResourceContext<'_>,
    ) -> Result<jqf_codec_core::DocumentProduct<'bytes>, CodecError> {
        let demand = CodecDemand::try_new(resources);
        let requirement = AccessRequirement::try_whole(
            demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            resources,
        )
        .expect("requirement")
        .with_fact_intent(jqf_codec_core::FactIntent::Preserve);
        decode_requirement_product(bytes, &requirement, resources)
    }

    fn decode_requirement_product<'bytes>(
        bytes: &'bytes [u8],
        requirement: &AccessRequirement,
        resources: &mut ResourceContext<'_>,
    ) -> Result<jqf_codec_core::DocumentProduct<'bytes>, CodecError> {
        let registration = crate::registration().expect("registration");
        let decoder = registration.decoder().expect("decoder");
        let mut provider: ErasedProvider = decoder.create_provider(source(bytes), simple_request(), resources)?;
        let handle = provider.bind(requirement).expect("bind");
        let mut session = provider.open(&handle, resources)?;
        let mut context = jqf_codec_core::CodecRunContext::new(resources);
        context.set_cooperative_credits(4_096);
        let result = session.decode(&mut context)?;
        let product = match result.outcome() {
            AccessOutcome::FullDocument(product) => product,
            AccessOutcome::Located(located) => located.product(),
        };
        product.try_clone().map_err(|_error| {
            CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "test product clone",
            })
        })
    }

    fn decode_located_node<'bytes>(
        bytes: &'bytes [u8],
        requirement: &AccessRequirement,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(jqf_codec_core::DocumentProduct<'bytes>, jqf_data::NodeHandle), CodecError> {
        let registration = crate::registration().expect("registration");
        let decoder = registration.decoder().expect("decoder");
        let mut provider: ErasedProvider = decoder.create_provider(source(bytes), simple_request(), resources)?;
        let handle = provider.bind(requirement).expect("bind");
        let mut session = provider.open(&handle, resources)?;
        let mut context = jqf_codec_core::CodecRunContext::new(resources);
        context.set_cooperative_credits(4_096);
        let result = session.decode(&mut context)?;
        let AccessOutcome::Located(located) = result.outcome() else {
            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "test expected located outcome",
            }));
        };
        let ExactSelectionRecord::Node { node, .. } = located.result() else {
            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "test expected located node",
            }));
        };
        let node = *node;
        let product = located.product().try_clone().map_err(|_error| {
            CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "test product clone",
            })
        })?;
        Ok((product, node))
    }

    fn whole_requirement(demand: CodecDemand, resources: &ResourceContext<'_>) -> AccessRequirement {
        AccessRequirement::try_whole(
            demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            resources,
        )
        .expect("requirement")
    }

    fn exact_member_requirement(
        member: &str,
        demand: CodecDemand,
        resources: &ResourceContext<'_>,
    ) -> AccessRequirement {
        let mut path = ExactPath::try_new(resources);
        path.try_push_semantic_member(member, resources).expect("member");
        let footprint = AccessFootprint::try_exact(path, resources);
        AccessRequirement::try_exact(
            footprint,
            demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            resources,
        )
        .expect("requirement")
    }

    fn attached_fact_demand(role: &str, resources: &ResourceContext<'_>) -> CodecDemand {
        let mut demand = CodecDemand::try_new(resources);
        let kind = FactKindId::try_new(role).expect("kind");
        let role = FactRoleId::try_new(role).expect("role");
        demand
            .try_insert(&DemandClause::AttachedFact { kind, role })
            .expect("insert");
        demand
    }

    fn tag_demand(resources: &ResourceContext<'_>) -> CodecDemand {
        let mut demand = CodecDemand::try_new(resources);
        demand.try_insert(&DemandClause::IntrinsicTag).expect("insert");
        demand
    }

    fn keep_member_tree(name: &str, resources: &ResourceContext<'_>) -> PruneTree {
        let mut tree = PruneTree::try_new(resources).expect("tree");
        let keep = tree.try_push_node(true).expect("keep");
        tree.try_push_key(PruneTree::ROOT, name, keep).expect("key");
        tree
    }

    fn object_keys(value: &Value) -> Vec<&str> {
        let Value::Object(object) = value else {
            panic!("expected object");
        };
        object.iter().map(jqf_data::ObjectEntry::key).collect()
    }

    /// Two distinct key texts sharing one FNV-1a fingerprint (a verified pair) must not hide an exact duplicate: the
    /// shortlist buckets CHAIN, so the repeated first key is still refused even though its colliding neighbour occupies
    /// the same slot. A single-slot map would overwrite the first occupant and let `{A, B, A}` publish.
    #[test]
    fn colliding_fingerprint_does_not_hide_an_exact_duplicate() {
        // The colliding texts alone are distinct keys and decode together.
        let value = decode(b"k9bf182762ea0cb9f: 1\nk9233634f513e85b8: 2\n").expect("decode");
        let Value::Object(object) = &value else {
            panic!("expected object");
        };
        assert_eq!(object.len(), 2);
        // The exact repeat of the first key is a duplicate despite the collision in between.
        let error = decode(b"k9bf182762ea0cb9f: 1\nk9233634f513e85b8: 2\nk9bf182762ea0cb9f: 3\n")
            .expect_err("exact duplicate behind a collision");
        assert_eq!(error.kind(), CodecFailureKind::InvalidInput);
    }

    /// The key-equivalence law compares RESOLVED tags: an explicit `!!str a` and a plain string `a` are one key, so
    /// their co-occurrence is a duplicate under yaml.key-equivalence@1 even though their explicit tag spellings differ.
    /// Comparing the explicit spellings would publish `{"a": 2}` instead.
    #[test]
    fn duplicate_key_across_explicit_and_implicit_str_tag_refuses() {
        let error = decode(b"{!!str a: 1, a: 2}\n").expect_err("duplicate key");
        assert_eq!(error.kind(), CodecFailureKind::InvalidInput);
    }

    /// The canonicalizer's vocabulary is exactly the schema's (`parse_yaml_int`): no binary prefix, no uppercase radix
    /// prefixes, no underscores, and a sign never precedes a radix form. Every caller resolves the Integer category
    /// first, so these spellings can only arrive through direct misuse — `None`, not a value.
    #[test]
    fn canonical_integer_vocabulary_matches_the_schema() {
        assert!(canonical_integer_for("0b101").is_none());
        assert!(canonical_integer_for("0B101").is_none());
        assert!(canonical_integer_for("0X1F").is_none());
        assert!(canonical_integer_for("0O17").is_none());
        assert!(canonical_integer_for("1_000").is_none());
        assert!(canonical_integer_for("-0x1F").is_none());
        assert!(canonical_integer_for("+0o17").is_none());
        // The accepted forms still canonicalize.
        assert_eq!(canonical_integer_for("0x1f").as_deref(), Some("31"));
        assert_eq!(canonical_integer_for("+010").as_deref(), Some("10"));
        assert_eq!(canonical_integer_for("-0").as_deref(), Some("0"));
    }

    #[test]
    fn radix_integers_beyond_u128_match_decimal() {
        // 2^132 exceeds u128::MAX (2^128 - 1); the exact-integer law makes every radix spelling canonicalize to the
        // same decimal value.
        let decimal = "5444517870735015415413993718908291383296";
        for spelling in [
            "0x1000000000000000000000000000000000",
            "0o100000000000000000000000000000000000000000000",
            "5444517870735015415413993718908291383296",
        ] {
            assert_eq!(canonical_integer_for(spelling).as_deref(), Some(decimal));
        }
        // The 32-hex-digit u128::MAX boundary is still distinct.
        assert_eq!(
            canonical_integer_for("0xffffffffffffffffffffffffffffffff").as_deref(),
            Some("340282366920938463463374607431768211455")
        );
        // And through the full decode path, a radix spelling equals its decimal spelling as a materialized value.
        let value = decode(b"a: 0x1000000000000000000000000000000000\nb: 5444517870735015415413993718908291383296\n")
            .expect("decode");
        let Value::Object(object) = &value else {
            panic!("expected object");
        };
        // Integer equality is by canonical text: the radix spelling must produce the same canonical decimal as the
        // decimal spelling.
        let int_text = |key: &str| -> String {
            let Value::Number(number) = object.get(key).expect(key) else {
                panic!("expected number at {key}");
            };
            number.to_integer().expect("integer").as_str().to_owned()
        };
        assert_eq!(int_text("a"), int_text("b"));
        assert_eq!(int_text("a"), decimal);
    }

    #[test]
    fn radix_literals_past_the_digit_bound_refuse() {
        // The conversion-cost narrowing: a radix spelling beyond `MAX_RADIX_DIGITS` significant digits refuses (the
        // caller raises `invalid integer literal`) while the same magnitude's decimal spelling canonicalizes. Leading
        // zeroes do not count toward the bound.
        let within = "1".repeat(MAX_RADIX_DIGITS);
        assert!(canonical_integer_for(&format!("0x{within}")).is_some());
        let beyond = "1".repeat(MAX_RADIX_DIGITS + 1);
        assert!(canonical_integer_for(&format!("0x{beyond}")).is_none());
        // A small value spelled with many leading zeroes stays under the bound.
        let padded = format!("{}{}", "0".repeat(MAX_RADIX_DIGITS + 8), "1");
        assert_eq!(canonical_integer_for(&format!("0x{padded}")).as_deref(), Some("1"));
    }

    /// A comment span computed in DECODED coordinates was sliced against the raw source bytes. The minimized reproducer
    /// is a NUL-prefixed `#=!` comment that decodes as UTF-16BE, followed by `\xff`/tab-heavy flow content: the decoded
    /// buffer is LONGER than the source, so the decoded-coordinate span end overshoots the source length and
    /// `comment_text`'s `&source[start..end]` panicked in every build. Each saved input must decode (or reject) without
    /// a panic.
    #[test]
    fn comment_span_on_a_decoded_source_never_overshoots_the_source() {
        // `corpus/crash-yaml-comment-span/min-104.bin` (104 bytes): NUL-prefixed comment + `\xff`/tab tail (verified
        // min form).
        let min_104: &[u8] = &[
            0x00, 0x23, 0x3d, 0x21, 0x00, 0xff, 0xd0, 0x00, 0x00, 0x2d, 0x2d, 0x2d, 0x0a, 0x2d, 0x20, 0x6e, 0x61, 0x6c,
            0x65, 0x3a, 0x20, 0x41, 0x4e, 0x63, 0x68, 0x6f, 0x72, 0x73, 0x20, 0x57, 0x1f, 0x1f, 0x1f, 0x1f, 0x1f, 0x1f,
            0x1f, 0x1f, 0x1f, 0x1f, 0x1f, 0x1f, 0x1f, 0x1f, 0x1f, 0x1f, 0x69, 0x1f, 0x74, 0x68, 0xbb, 0x43, 0x6f, 0x6c,
            0x6f, 0x6e, 0x20, 0x69, 0xff, 0xff, 0xff, 0xff, 0x64, 0x65, 0x64, 0x20, 0x6c, 0x69, 0x6e, 0x3a, 0x20, 0x75,
            0x6c, 0x6c, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x65, 0x0a, 0x20, 0x20, 0x29, 0x3a, 0x0a,
        ];
        // `corpus/crash-yaml-comment-span/artifact-113.bin` (113 bytes): the same NUL-prefixed UTF-16BE comment, a
        // longer `\xff`/tab tail.
        let artifact_113: &[u8] = &[
            0x00, 0x23, 0x3d, 0x21, 0x00, 0xff, 0xd0, 0x00, 0x00, 0x2d, 0x2d, 0x2d, 0x0a, 0x2d, 0x20, 0x6e, 0x61, 0x6c,
            0x65, 0x3a, 0x20, 0x41, 0x4e, 0x63, 0x68, 0x6f, 0x72, 0x73, 0x20, 0x57, 0x1f, 0x1f, 0x1f, 0x1f, 0x1f, 0x1f,
            0x1f, 0x1f, 0x1f, 0x1f, 0x1f, 0x1f, 0x1f, 0x1f, 0x1f, 0x1f, 0x69, 0x1f, 0x74, 0x68, 0xbb, 0x43, 0x6f, 0x6c,
            0x6f, 0x6e, 0x20, 0x69, 0xff, 0xff, 0xff, 0xff, 0x64, 0x65, 0x64, 0x20, 0x6c, 0x69, 0x6e, 0x3a, 0x20, 0x75,
            0x6c, 0x6c, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x65, 0x0a, 0x20, 0x20, 0x29, 0x20, 0x20, 0x66, 0x6f, 0x6f, 0x3a,
            0x20, 0x2a, 0x61, 0x3a, 0x0a,
        ];
        // `corpus/crash-yaml-comment-span/artifact-605.bin` (605 bytes): a UTF-16LE decoded source (the `01 00` lead)
        // with an embedded NUL comment and `\xff`/`\x0b` tails.
        let artifact_605: &[u8] = &[
            0x01, 0x00, 0x2d, 0x0a, 0x2d, 0x61, 0x6d, 0x65, 0x3a, 0x20, 0x42, 0x6c, 0x6f, 0x63, 0x6b, 0x20, 0x4d, 0x61,
            0x70, 0x70, 0x69, 0x6e, 0x67, 0x20, 0x77, 0x69, 0x74, 0x68, 0x20, 0x4d, 0x69, 0x73, 0x73, 0x69, 0x2d, 0x2d,
            0x2d, 0x0a, 0x2d, 0x20, 0x6e, 0x61, 0x6d, 0x65, 0x3a, 0x20, 0x53, 0x70, 0x65, 0x63, 0x36, 0x36, 0x36, 0x36,
            0x36, 0x36, 0x36, 0x36, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x0b, 0x53, 0x45, 0x51, 0x0a, 0x20,
            0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x3d, 0x56, 0x41, 0x4c, 0x20, 0x3a, 0x62, 0x61, 0x7a, 0x0a, 0x00,
            0x23, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x2d, 0x2d, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x2d, 0x2d, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x60, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2d, 0x0a, 0x2d, 0x20,
            0x20, 0x20, 0x61, 0x67, 0x73, 0x3a, 0x20, 0x73, 0x70, 0x65, 0x63, 0x20, 0x66, 0x6f, 0x6e, 0x67, 0x20, 0x56,
            0x61, 0x6c, 0x75, 0x65, 0x73, 0x0a, 0x20, 0x20, 0x66, 0x72, 0x6f, 0x6d, 0x3a, 0x20, 0x4e, 0x69, 0x6d, 0x59,
            0x41, 0x4d, 0x4c, 0x20, 0x74, 0x65, 0x73, 0x74, 0x73, 0x0a, 0x20, 0x22, 0x74, 0x61, 0x67, 0x73, 0x3a, 0x20,
            0x65, 0x78, 0x70, 0x6c, 0x69, 0x63, 0x69, 0x74, 0x2d, 0x6b, 0x65, 0x79, 0x20, 0x6d, 0x61, 0x70, 0x58, 0x69,
            0x6e, 0x67, 0x0a, 0x20, 0x20, 0x79, 0x61, 0x6d, 0x6c, 0x3a, 0x20, 0x7c, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x3f,
            0x20, 0x61, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x3f, 0x20, 0x62, 0x02, 0x20, 0x20, 0x20, 0x20, 0x63, 0x3a, 0x0a,
            0x20, 0x20, 0x74, 0x72, 0x65, 0x65, 0x3a, 0x20, 0x7c, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x2b, 0x53, 0x54, 0x52,
            0x0a, 0x20, 0x20, 0x20, 0x20, 0x20, 0x2b, 0x44, 0x4f, 0x43, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x26,
            0x49, 0x41, 0x50, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x3d, 0x56, 0x41, 0x4c, 0x20, 0x3a, 0x61,
            0x0a, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x3d, 0x56, 0x41, 0x4c, 0x20, 0x3a, 0x0a, 0x20, 0x20, 0x20,
            0x20, 0x20, 0x20, 0x20, 0x3d, 0x56, 0x41, 0x4c, 0x20, 0x3a, 0x62, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x20, 0x22,
            0x61, 0x22, 0x3a, 0x20, 0x6e, 0x75, 0x6c, 0x6c, 0x2c, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x22, 0x62,
            0x22, 0x3a, 0x20, 0x6e, 0x75, 0x6c, 0x6c, 0x2c, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
            0x20, 0x20, 0x3d, 0x56, 0x41, 0x4c, 0x20, 0x3a, 0x61, 0x6c, 0x64, 0x65, 0x64, 0x20, 0x73, 0x63, 0x61, 0x6c,
            0x61, 0x72, 0x20, 0x63, 0x6f, 0x6d, 0x6d, 0x65, 0x6e, 0x74, 0x20, 0x31, 0x2e, 0x33, 0x2d, 0x65, 0x72, 0x72,
            0x0a, 0x20, 0x20, 0x79, 0x61, 0x6d, 0x6c, 0x3a, 0x20, 0x7c, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
            0x3d, 0x56, 0x3a, 0x20, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x3e, 0x7c, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x61, 0x3a,
            0x0a, 0x20, 0x20, 0x20, 0x16, 0x62, 0x3a, 0x0a, 0x20, 0x20, 0x20, 0x05, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0x3f, 0x0a, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x20, 0x66, 0x6f, 0x20, 0x6c, 0x64, 0x65, 0x64, 0x0a, 0x63,
            0x20, 0x20, 0x3a, 0x0a, 0x20,
        ];
        // `corpus/crash-yaml-comment-span/artifact-113.bin` (113 bytes) and `artifact-605.bin` (605 bytes) are the
        // campaign's other two saves.
        for (name, input) in [
            ("min-104", min_104),
            ("artifact-113", artifact_113),
            ("artifact-605", artifact_605),
        ] {
            // Each save is a legitimate (if hostile) UTF-16 YAML document; the defect was the PANIC. min-104 decodes
            // cleanly — the comment facts are simply absent (no source-sliceable span). The two ODD-length saves end in
            // a partial code unit and are now a clean typed refusal instead of a silently dropped tail byte; either
            // way, never a panic.
            match decode(input) {
                Ok(_) => {}
                Err(error) => {
                    assert_eq!(error.kind(), CodecFailureKind::InvalidInput, "{name}");
                }
            }
        }
    }

    #[test]
    fn cyclic_alias_fails_instead_of_building() {
        // `a: &x [*x]` and `a: &x {b: *x}`: the alias re-enters a node still on the recursion path — a cycle, never a
        // semantic value. The build must fail with UnsupportedRepresentation (the module doc's promise), not publish a
        // self-referential document.
        for yaml in [b"a: &x [*x]\n" as &[u8], b"a: &x {b: *x}\n"] {
            let error = decode(yaml).expect_err("cyclic alias must fail");
            assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
        }
    }

    #[test]
    fn dag_aliases_still_share_one_node() {
        // Two aliases to one completed anchor stay legal: the anchored mapping is popped from `in_progress` before the
        // second alias, so both aliases memo-hit the same node.
        let mut resources = resources();
        let product = decode_product(b"base: &x {n: 1}\na: *x\nb: *x\n", &mut resources).expect("decode");
        let base = node_at(&product, &["base"]);
        assert_eq!(base, node_at(&product, &["a"]));
        assert_eq!(base, node_at(&product, &["b"]));
    }

    #[test]
    fn nested_mapping_keys_do_not_leak_into_parent_duplicate_check() {
        // The nested `{b: 1}` walk used to clear the parent's shared seen-keys vec and then leave its own `b` behind,
        // so the parent's later `b: 2` was falsely rejected as a duplicate. The parent key `b` and the nested key `b`
        // are distinct.
        let value = decode(b"a: {b: 1}\nb: 2\n").expect("decode");
        let Value::Object(object) = &value else {
            panic!("expected object");
        };
        let Value::Object(nested) = object.get("a").expect("a") else {
            panic!("expected nested object at a");
        };
        let Value::Number(nested_b) = nested.get("b").expect("a.b") else {
            panic!("expected number at a.b");
        };
        assert_eq!(nested_b.to_integer().expect("integer").as_str(), "1");
        let Value::Number(b) = object.get("b").expect("b") else {
            panic!("expected number at b");
        };
        assert_eq!(b.to_integer().expect("integer").as_str(), "2");
    }

    #[test]
    fn real_duplicate_key_still_fails_under_equivalence_law() {
        // The genuine duplicate at the SAME mapping level must still raise the duplicate-key error.
        let error = decode(b"a: 1\na: 2\n").expect_err("duplicate key must fail");
        assert_eq!(error.kind(), CodecFailureKind::InvalidInput);
        let duplicate_nested = decode(b"a: {b: 1, b: 2}\n").expect_err("nested duplicate must fail");
        assert_eq!(duplicate_nested.kind(), CodecFailureKind::InvalidInput);
    }

    // ---- the edit-lane span and refusal-fact laws ----

    /// The document node at the end of `path` (object members only), for the span/fact assertions.
    fn node_at(product: &jqf_codec_core::DocumentProduct<'_>, path: &[&str]) -> jqf_data::NodeId {
        let document = product.document();
        let mut node = document.root_handle();
        for key in path {
            let object = document
                .value_view(node)
                .expect("view")
                .object()
                .expect("object")
                .expect("object view");
            let mut found = None;
            for entry in object.iter() {
                let entry = entry.expect("entry");
                if entry.key() == *key {
                    found = Some(entry.value().node());
                    break;
                }
            }
            node = document.node_handle(found.expect("path member")).expect("handle");
        }
        document.value_view(node).expect("final view").node()
    }

    #[test]
    fn block_scalar_retains_its_whole_block_span() {
        // A literal block scalar's authored span is the whole `|` block (header through last content line, trailing
        // break trimmed): the ruled "a block-scalar edit replaces the whole scalar span".
        let mut resources = resources();
        let product = decode_product(b"b: |\n  line1\n  line2\nnext: 1\n", &mut resources).expect("decode");
        let document = product.document();
        let span = document
            .node_source_span(node_at(&product, &["b"]))
            .expect("span")
            .expect("block scalar must retain a span");
        assert_eq!(span.start() as usize, 3, "starts at the `|`");
        assert_eq!(span.end() as usize, 20, "ends after `line2`, before the break");
        let bytes = b"b: |\n  line1\n  line2\nnext: 1\n";
        assert_eq!(
            &bytes[span.start() as usize..span.end() as usize],
            b"|\n  line1\n  line2"
        );
    }

    #[test]
    fn trim_trailing_break_uses_the_last_utf8_byte() {
        let nel = [b'x', 0xC2, 0x85];
        assert_eq!(super::trim_trailing_break(&nel, 3), 1);
        let line_separator = [b'x', 0xE2, 0x80, 0xA8];
        assert_eq!(super::trim_trailing_break(&line_separator, 4), 1);
        let paragraph_separator = [b'x', 0xE2, 0x80, 0xA9];
        assert_eq!(super::trim_trailing_break(&paragraph_separator, 4), 1);
        assert_eq!(super::trim_trailing_break(b"x\n", 2), 1);
    }

    #[test]
    fn comment_text_keeps_further_spaces_and_tabs() {
        // Extraction removes the `#` delimiter and line terminator, then EXACTLY ONE immediately following ASCII space
        // when present; every remaining scalar (further spaces, tabs) is text. `trim()` stripped all of it, so `# two
        // spaces` read `"two spaces"` and wrote back `# two spaces` — a broken round-trip.
        let source = b"#  two spaces\nk: 1\n";
        let span = jqf_source::Span::from_usize(0, 13);
        assert_eq!(comment_text(source, span), " two spaces");
        let source = b"#\ttab\nk: 1\n";
        let span = jqf_source::Span::from_usize(0, 5);
        assert_eq!(comment_text(source, span), "\ttab");
    }

    #[test]
    fn identity_demand_does_not_attach_comment_facts() {
        let mut resources = resources();
        let requirement = whole_requirement(CodecDemand::try_new(&resources), &resources);
        let product = decode_requirement_product(b"# lead a\na: 1\n", &requirement, &mut resources).expect("decode");
        assert!(
            comment_facts(&product, &["a"], COMMENT_FACT).is_empty(),
            "identity must skip comment facts"
        );
    }

    #[test]
    fn identity_demand_keeps_tagged_values_and_skips_core_tags() {
        let mut resources = resources();
        let requirement = whole_requirement(CodecDemand::try_new(&resources), &resources);
        let tagged = decode_requirement_product(b"!money \"10\"\n", &requirement, &mut resources).expect("decode");
        assert_eq!(
            tagged
                .document()
                .value_view(tagged.document().root_handle())
                .expect("view")
                .tag()
                .expect("tags available")
                .map(jqf_data::TagId::as_str),
            Some("!money"),
            "identity must keep a non-core Tagged value"
        );
        let core = decode_requirement_product(b"a: true\n", &requirement, &mut resources).expect("decode");
        assert!(
            node_tag(&core, &["a"]).is_none(),
            "identity must skip core intrinsic tags"
        );
    }

    #[test]
    fn comment_clause_attaches_comment_facts() {
        let mut resources = resources();
        let requirement = whole_requirement(attached_fact_demand("comment", &resources), &resources);
        let product = decode_requirement_product(b"# lead a\na: 1\n", &requirement, &mut resources).expect("decode");
        assert_eq!(comment_facts(&product, &["a"], COMMENT_FACT), vec!["lead a"]);
    }

    #[test]
    fn tag_clause_attaches_core_tags() {
        let mut resources = resources();
        let requirement = whole_requirement(tag_demand(&resources), &resources);
        let product = decode_requirement_product(b"a: true\n", &requirement, &mut resources).expect("decode");
        assert_eq!(node_tag(&product, &["a"]).as_deref(), Some(crate::schema::TAG_BOOL));
    }

    #[test]
    fn exact_identity_demand_skips_comments_and_keeps_tagged_values() {
        let mut resources = resources();
        let empty = CodecDemand::try_new(&resources);
        let comments = exact_member_requirement("a", empty, &resources);
        let product = decode_requirement_product(b"# lead a\na: 1\n", &comments, &mut resources).expect("decode");
        assert!(
            comment_facts(&product, &[], COMMENT_FACT).is_empty(),
            "Exact identity must skip comment facts"
        );
        let tagged_req = exact_member_requirement("a", CodecDemand::try_new(&resources), &resources);
        let tagged = decode_requirement_product(b"a: !money \"10\"\n", &tagged_req, &mut resources).expect("decode");
        assert_eq!(
            tagged
                .document()
                .value_view(tagged.document().root_handle())
                .expect("view")
                .tag()
                .expect("tags available")
                .map(jqf_data::TagId::as_str),
            Some("!money"),
            "Exact identity must keep a non-core Tagged value"
        );
    }

    #[test]
    fn whole_prune_omits_unobservable_members() {
        let mut resources = resources();
        let requirement = whole_requirement(CodecDemand::try_new(&resources), &resources)
            .with_prune(keep_member_tree("id", &resources));
        let product = decode_requirement_product(
            b"id: 1\nname: extra\nnested:\n  deep: 1\n",
            &requirement,
            &mut resources,
        )
        .expect("decode");
        let value = product
            .document()
            .materialize_root(&mut resources)
            .expect("materialize");
        assert_eq!(object_keys(&value), ["id"]);
        // Mapping + kept key node + kept value. Omitted siblings are not built.
        assert_eq!(product.document().node_count(), 3);
        let error = decode_requirement_product(b"id: 1\nname: [\n", &requirement, &mut resources)
            .expect_err("omitted members still validate");
        assert_eq!(error.kind(), CodecFailureKind::InvalidInput);
    }

    #[test]
    fn exact_prune_omits_unread_members_of_the_located_object() {
        let mut resources = resources();
        let requirement = exact_member_requirement("catalog", CodecDemand::try_new(&resources), &resources)
            .with_prune(keep_member_tree("id", &resources));
        let product = decode_requirement_product(
            b"catalog:\n  id: 1\n  name: extra\n  nested:\n    deep: 1\n",
            &requirement,
            &mut resources,
        )
        .expect("decode");
        let value = product
            .document()
            .materialize_root(&mut resources)
            .expect("materialize");
        assert_eq!(object_keys(&value), ["id"]);
        // Located mapping + kept value. The scoped walk never builds key nodes.
        assert_eq!(product.document().node_count(), 2);
        let error = decode_requirement_product(b"catalog:\n  id: 1\n  name: [\n", &requirement, &mut resources)
            .expect_err("omitted members still validate");
        assert_eq!(error.kind(), CodecFailureKind::InvalidInput);
    }

    fn yaml_container_count() -> CountDemand {
        CountDemand {
            row: CountRow::Container,
            path: alloc::vec::Vec::new(),
            range: None,
            probe: alloc::vec::Vec::new(),
            filter: None,
        }
    }

    /// YAML native Exact publishes a subtree whose root is `.users`. Oracle
    /// count starts at `located.node()` with an empty path. Whole of the same
    /// bytes keeps the mapping: empty-path count is 1. `node == root` on the
    /// Exact product is not Whole.
    #[test]
    fn exact_users_length_goes_through_document_oracle() {
        let mut resources = resources();
        let requirement = exact_member_requirement("users", CodecDemand::try_new(&resources), &resources);
        let (product, node) =
            decode_located_node(b"users:\n- 1\n- 2\n- 3\n", &requirement, &mut resources).expect("YAML Exact decodes");
        let document = product.document();
        assert_eq!(
            node,
            document.root_handle(),
            "YAML native Exact republishes the subtree as root"
        );
        assert_eq!(
            document
                .count_children_from(node, &yaml_container_count(), &mut resources)
                .expect("oracle count"),
            CountVerdict::Count(3)
        );
        let rewalk = CountDemand {
            row: CountRow::Container,
            path: alloc::vec![CountStep::ObjectKey(alloc::string::String::from("users"))],
            range: None,
            probe: alloc::vec::Vec::new(),
            filter: None,
        };
        assert_eq!(
            document
                .count_children_from(document.root_handle(), &rewalk, &mut resources)
                .expect("rewalk"),
            CountVerdict::Decline,
            "walking PATH again on a republished sequence declines"
        );
        let whole = decode_product(b"users:\n- 1\n- 2\n- 3\n", &mut resources).expect("YAML Whole decodes");
        assert_eq!(
            whole
                .document()
                .count_children_from(whole.document().root_handle(), &yaml_container_count(), &mut resources)
                .expect("Whole empty path"),
            CountVerdict::Count(1),
            "Whole empty path is the mapping member count"
        );
    }

    #[test]
    fn prune_keeps_merge_shared_nodes_whole_for_a_later_alias() {
        // `<<: *d` reuses `d`'s value nodes. A prune that keeps `.svc.v.p` and
        // `.zz` (whole) must not memoize the pruned `{p}` and serve it to `zz`.
        let mut resources = resources();
        let mut tree = PruneTree::try_new(&resources).expect("tree");
        let p = tree.try_push_node(true).expect("p");
        let v = tree.try_push_node(false).expect("v");
        tree.try_push_key(v, "p", p).expect("p key");
        let svc = tree.try_push_node(false).expect("svc");
        tree.try_push_key(svc, "v", v).expect("v key");
        let zz = tree.try_push_node(true).expect("zz");
        tree.try_push_key(PruneTree::ROOT, "svc", svc).expect("svc key");
        tree.try_push_key(PruneTree::ROOT, "zz", zz).expect("zz key");
        let requirement = whole_requirement(CodecDemand::try_new(&resources), &resources).with_prune(tree);
        let product = decode_requirement_product(
            b"defaults: &d\n  v:\n    p: 1\n    q: 2\nsvc:\n  <<: *d\nzz: *d\n",
            &requirement,
            &mut resources,
        )
        .expect("decode");
        let value = product
            .document()
            .materialize_root(&mut resources)
            .expect("materialize");
        let Value::Object(root) = &value else {
            panic!("expected object");
        };
        let zz = root.iter().find(|entry| entry.key() == "zz").expect("zz").value();
        let Value::Object(zz) = zz else {
            panic!("expected zz object");
        };
        let v = zz.iter().find(|entry| entry.key() == "v").expect("v").value();
        let Value::Object(v) = v else {
            panic!("expected v object");
        };
        let keys: Vec<&str> = v.iter().map(jqf_data::ObjectEntry::key).collect();
        assert_eq!(keys, ["p", "q"], "alias site must keep the whole shared mapping");
    }

    #[test]
    fn prune_keeps_alias_shared_nodes_whole_for_a_later_alias() {
        let mut resources = resources();
        let mut tree = PruneTree::try_new(&resources).expect("tree");
        let p = tree.try_push_node(true).expect("p");
        let pruned = tree.try_push_node(false).expect("pruned");
        tree.try_push_key(pruned, "p", p).expect("p key");
        let kept = tree.try_push_node(true).expect("kept");
        tree.try_push_key(PruneTree::ROOT, "kept", kept).expect("kept key");
        tree.try_push_key(PruneTree::ROOT, "pruned", pruned)
            .expect("pruned key");
        let requirement = whole_requirement(CodecDemand::try_new(&resources), &resources).with_prune(tree);
        let product = decode_requirement_product(
            b"base: &d\n  p: 1\n  q: 2\npruned: *d\nkept: *d\n",
            &requirement,
            &mut resources,
        )
        .expect("decode");
        let value = product
            .document()
            .materialize_root(&mut resources)
            .expect("materialize");
        let Value::Object(root) = &value else {
            panic!("expected object");
        };
        let kept = root.iter().find(|entry| entry.key() == "kept").expect("kept").value();
        let Value::Object(kept) = kept else {
            panic!("expected kept object");
        };
        let keys: Vec<&str> = kept.iter().map(jqf_data::ObjectEntry::key).collect();
        assert_eq!(keys, ["p", "q"], "later alias site must keep the whole shared mapping");
    }

    fn node_tag(product: &jqf_codec_core::DocumentProduct<'_>, path: &[&str]) -> Option<String> {
        let document = product.document();
        let handle = document.node_handle(node_at(product, path)).expect("handle");
        document
            .value_view(handle)
            .expect("view")
            .tag()
            .expect("tags available")
            .map(|tag| String::from(tag.as_str()))
    }

    /// Collects one comment role's texts attached to one document node.
    fn comment_facts(product: &jqf_codec_core::DocumentProduct<'_>, path: &[&str], role: &str) -> Vec<String> {
        let document = product.document();
        let node = node_at(product, path);
        let mut out = Vec::new();
        for fact_id in document.owner_fact_ids(node) {
            let fact = document.fact(*fact_id).expect("fact");
            if fact.role().as_str() != role {
                continue;
            }
            if let FactPayloadView::List(items) = fact.payload() {
                for item in items.iter() {
                    if let FactPayloadView::Text(text) = item {
                        out.push(String::from(text));
                    }
                }
            }
        }
        out
    }

    #[test]
    fn comment_positions_classify_inline_foot_and_trailer() {
        // The same-line comment attaches to its own value as `yaml.comment_inline@1`; `.@comment` is leading-only, so
        // `.b.@comment` no longer absorbs it; and the document trailer is the root's `yaml.comment_foot@1`, never the
        // root's leading.
        let mut resources = resources();
        let product = decode_product(
            b"# lead a\na: 1 # inline-a\n# lead b\nb: 2\n# trailer\n",
            &mut resources,
        )
        .expect("decode");
        assert_eq!(comment_facts(&product, &["a"], COMMENT_FACT), vec!["lead a"]);
        assert_eq!(comment_facts(&product, &["a"], COMMENT_INLINE_FACT), vec!["inline-a"]);
        assert_eq!(comment_facts(&product, &["b"], COMMENT_FACT), vec!["lead b"]);
        assert_eq!(comment_facts(&product, &[], COMMENT_FACT), Vec::<String>::new());
        assert_eq!(comment_facts(&product, &[], COMMENT_FOOT_FACT), vec!["trailer"]);

        // The deeper-indented comment is the closing block's foot on `a`, never `b`'s leading; the flush-left comment
        // stays the next node's leading.
        let product = decode_product(b"a:\n  x: 1\n  # foot of a\nb: 2\n# trailer\n", &mut resources).expect("decode");
        assert_eq!(comment_facts(&product, &["a"], COMMENT_FACT), Vec::<String>::new());
        assert_eq!(comment_facts(&product, &["a"], COMMENT_FOOT_FACT), vec!["foot of a"]);
        assert_eq!(comment_facts(&product, &["b"], COMMENT_FACT), Vec::<String>::new());
        assert_eq!(comment_facts(&product, &[], COMMENT_FOOT_FACT), vec!["trailer"]);
        let product = decode_product(b"a: 1\n# lead of b\nb: 2\n", &mut resources).expect("decode");
        assert_eq!(comment_facts(&product, &["b"], COMMENT_FACT), vec!["lead of b"]);
        assert_eq!(comment_facts(&product, &["b"], COMMENT_FOOT_FACT), Vec::<String>::new());
    }

    #[test]
    fn block_container_retains_first_entry_to_subtree_span() {
        // A block mapping's authored span runs from its first entry's start to its last member's whole-subtree end —
        // the region and indentation the structural append closes on.
        let mut resources = resources();
        let product = decode_product(b"a:\n  b: 1\n  c:\n    x: 2\n", &mut resources).expect("decode");
        let document = product.document();
        let span = document
            .node_source_span(node_at(&product, &["a"]))
            .expect("span")
            .expect("block mapping must retain a span");
        let bytes = b"a:\n  b: 1\n  c:\n    x: 2\n";
        assert_eq!(span.start() as usize, 5, "starts at the first key");
        assert_eq!(
            &bytes[span.start() as usize..span.end() as usize],
            b"b: 1\n  c:\n    x: 2",
            "ends at the last member's subtree end, trailing break trimmed"
        );
    }

    #[test]
    fn alias_shared_nodes_carry_the_edit_refusal_fact() {
        // Every node an alias references carries the format-neutral `edit-refusal` attached fact with the prose payload
        // (the alias-refusal law); an ordinary node carries none.
        let mut resources = resources();
        let product = decode_product(b"a: &x 1\nb: *x\nc: 3\n", &mut resources).expect("decode");
        let document = product.document();
        assert!(
            document
                .owner_fact_ids(node_at(&product, &["a"]))
                .iter()
                .any(|fact_id| {
                    document.fact(*fact_id).expect("fact").role().as_str() == jqf_codec_core::EDIT_REFUSAL_ROLE
                }),
            "the alias target (a) must carry the refusal fact"
        );
        assert!(
            document
                .owner_fact_ids(node_at(&product, &["c"]))
                .iter()
                .all(|fact_id| {
                    document.fact(*fact_id).expect("fact").role().as_str() != jqf_codec_core::EDIT_REFUSAL_ROLE
                }),
            "an ordinary node must carry no refusal fact"
        );
    }

    #[test]
    fn merged_entries_carry_the_merge_override_fact_naming_their_host() {
        // A `<<:`-spliced member's value node carries the format-neutral `merge-override` attached fact whose payload
        // is the HOST mapping's document node id — the container a write to the member must splice into. The ANCHOR's
        // own entry (the same shared node, `defaults.timeout`) carries NO merge fact, so a write through the anchor is
        // still a plain alias write and stays refused; a node merged into TWO hosts carries one fact per host, so each
        // host's write finds its own.
        let mut resources = resources();
        let product = decode_product(
            b"defaults: &defaults\n  timeout: 30\nsvc_a:\n  <<: *defaults\n  name: a\nsvc_b:\n  <<: *defaults\n  name: b\n",
            &mut resources,
        )
        .expect("decode");
        let document = product.document();
        let host_a = node_at(&product, &["svc_a"]);
        let host_b = node_at(&product, &["svc_b"]);
        let merged = node_at(&product, &["svc_a", "timeout"]);
        let anchor_timeout = node_at(&product, &["defaults", "timeout"]);
        assert_eq!(merged, anchor_timeout, "the merged entry reuses the anchored node id");
        let mut payloads = Vec::new();
        for fact_id in document.owner_fact_ids(merged) {
            let fact = document.fact(*fact_id).expect("fact");
            if fact.role().as_str() == jqf_codec_core::MERGE_OVERRIDE_ROLE {
                match fact.payload() {
                    FactPayloadView::Integer(text) => payloads.push(text.to_owned()),
                    _ => panic!("unexpected merge-override payload (not an integer node id)"),
                }
            }
        }
        payloads.sort();
        let mut expected = vec![host_a.get().to_string(), host_b.get().to_string()];
        expected.sort();
        assert_eq!(
            payloads, expected,
            "one fact per host, payload = the host mapping's node id"
        );
        assert!(
            !payloads.contains(&node_at(&product, &["defaults"]).get().to_string()),
            "the fact never names the ANCHOR mapping itself: a write descending \
             through `defaults` sees no matching host and stays a refusal, \
             while each merge site sees its own"
        );
    }
}
