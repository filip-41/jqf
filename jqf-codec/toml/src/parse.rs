//! Single-pass TOML parser and semantic document construction.
//!
//! The parser is a straight-line `decode` state machine over a borrowed source, exactly like the strict-JSON parser: it
//! validates the complete grammar and table-definition state, then builds the format-neutral semantic `Document` and
//! returns it as an [`AccessResult`]. It builds the document in bounded cooperative chunks; each loop head admits work
//! against the request's [`jqf_resource::WorkMeter`] and replenishes the cooperative budget when it is exhausted, so a
//! long document stays cancellation- and deadline-observant without blocking the caller.
//!
//! The grammar states mirror TOML's ABNF: keys (bare/basic/literal, dotted), strings (basic/literal,
//! single/multi-line), numbers (decimal/hex/octal/ binary, floats, `inf`/`nan`), temporals (RFC 3339), arrays, inline
//! tables, headers (`[path]` / `[[path]]`), and trivia (`#` comments).
//!
//! Table-definition semantics (TOML spec): dotted keys contribute one logical path; `[a.b]` opens table `a.b`; a table
//! already defined cannot be reopened; `[[a]]` appends a new element to array-of-tables `a`; a dotted key cannot
//! traverse an array-of-tables; duplicate keys within one table are errors; incomplete EOF state terminates before
//! publication.
//!
//! The parse phase's working allocations (keys, comment text, the `Doc` state, the intermediate `Tree`) are
//! INPUT-bounded and uncharged: the request's memory accounting begins at the document BUILD, whose arena reserve
//! (`reserve_estimate`) is the ceiling's first contact. A pathological source can therefore allocate its tree before
//! the governor refuses — the peak is bounded by the `u32::MAX` source guard times the per-node cost, never
//! unbounded.

use alloc::string::String;

use jqf_codec_core::{
    AccessInput, AccessOutcome, AccessResult, AccessSession, CodecError, CodecFailureKind, CodecRunContext,
    DocumentProduct, PRUNE_ALL, PruneRef,
};

pub(crate) use jqf_codec_core::PruneLookup;
use jqf_data::{
    AccountedDocumentBuilder, AccountedDocumentFinalizer, AccountedOccurrenceKey, AccountedSemanticNode,
    AuthoritativeEmptyFamilies, BuilderCoverage, DataError, DiagnosticCoverage, DocumentCapabilityFamily,
    DocumentFinalizationPoll, DocumentSchemaRecipe, DocumentSourceBindingStage, FactPayload, LocalOwnerRef, NodeId,
    PreparedDocumentSchema, PreparedOccurrenceRole, PreparedSemanticNode,
};
use jqf_resource::ResourceContext;
use jqf_source::Span;

use crate::grammar::{Key, TextSource, Tree};
use crate::provider::DialectKind;

pub(crate) const TABLE_ROLE: &str = "toml.member@1";
pub(crate) const INLINE_ROLE: &str = "toml.inline-member@1";
pub(crate) const ARRAY_ROLE: &str = "toml.array-item@1";
/// The cross-format comment fact: one list-payload fact per key's value (and per table header) whose statements carried
/// LEADING comments. Serves `.@comment` (leading only; the own-line trailing comment is a separate inline fact). The
/// spelling is the shared vocabulary's `HEAD` segment under this codec's namespace; the
/// `comment_roles_agree_with_the_shared_vocabulary` test pins it.
pub(crate) const COMMENT_FACT: &str = "toml.comment@1";
/// The own-line trailing comment after a statement's value: one list-payload fact on the statement's value node. Serves
/// `.@comment_inline`.
pub(crate) const COMMENT_INLINE_FACT: &str = "toml.comment_inline@1";
/// A comment run between a table's last statement and the next `[header]`: one list-payload fact on the preceding
/// table's node. Serves `.@comment_foot`. The document trailer stays on the ROOT as [`COMMENT_FACT`], not here.
pub(crate) const COMMENT_FOOT_FACT: &str = "toml.comment_foot@1";

/// The codec's comment role constants are the shared vocabulary's spellings: the builders and the `'static` literals
/// cannot drift apart.
#[cfg(test)]
mod comment_vocabulary_tests {
    use super::{COMMENT_FACT, COMMENT_FOOT_FACT, COMMENT_INLINE_FACT};

    #[test]
    fn comment_roles_agree_with_the_shared_vocabulary() {
        use jqf_codec_core::comment;
        assert_eq!(COMMENT_FACT, comment::comment_role("toml"));
        assert_eq!(COMMENT_INLINE_FACT, alloc::format!("toml.{}@1", comment::INLINE));
        assert_eq!(COMMENT_FOOT_FACT, alloc::format!("toml.{}@1", comment::FOOT));
    }
}

fn toml_schema_recipe() -> Result<DocumentSchemaRecipe<'static>, DataError> {
    DocumentSchemaRecipe::try_new(
        "toml",
        Some("toml"),
        &["toml.table@1", "toml.inline-table@1", "toml.array@1", "toml.scalar@1"],
        &[TABLE_ROLE, INLINE_ROLE, ARRAY_ROLE],
        &[COMMENT_FACT, COMMENT_INLINE_FACT, COMMENT_FOOT_FACT],
        &[COMMENT_FACT, COMMENT_INLINE_FACT, COMMENT_FOOT_FACT],
    )
}

pub(crate) fn map_data(error: DataError) -> CodecError {
    jqf_codec_core::map_data(error, "TOML builder rejected document construction")
}

/// The TOML access session: one source, one document.
pub(crate) struct TomlParseState {
    /// The dialect grammar (the span materializer is dialect-specific).
    dialect: DialectKind,
    /// The forced container-span frontier depth, when the requirement asked for deferral.
    lazy_frontier: Option<usize>,
    /// The kept-subtree prune hint: which members the requesting program provably reads. `None` keeps everything.
    prune: Option<PruneLookup>,
    /// Builder coverage the document walk honours: skip attaching comments unless facts are demanded.
    coverage: BuilderCoverage,
    phase: Phase,
    /// The resumable grammar machine while the document is being parsed.
    grammar: Option<crate::grammar::TomlGrammar>,
    /// The in-flight builder once the parse completed.
    builder: Option<AccountedDocumentBuilder<'static>>,
    /// The builder's root node.
    root: Option<NodeId>,
    /// The in-flight cooperative source seal, when the parse committed spans.
    binding_stage: Option<DocumentSourceBindingStage>,
    /// Whether the parse bound a source seal (and so must finalize and publish through the source-backed arms).
    bound: bool,
    /// The in-flight cooperative finalizer.
    finalizer: Option<AccountedDocumentFinalizer<'static>>,
    /// Whether the parse completed and the product is published.
    published: bool,
    /// The completed document product.
    product: Option<DocumentProduct<'static>>,
}

enum Phase {
    /// The grammar parse and document build are pending.
    Parse,
    /// Sealing the source segment the committed spans name.
    Seal,
    /// Finalizing the built document.
    Finalize,
    /// The product is ready to publish.
    Publish,
}

impl TomlParseState {
    pub(crate) fn try_new(
        dialect: DialectKind,
        lazy_frontier: Option<usize>,
        prune: Option<PruneLookup>,
        coverage: BuilderCoverage,
    ) -> Self {
        Self {
            dialect,
            lazy_frontier,
            prune: prune.clone(),
            coverage,
            phase: Phase::Parse,
            grammar: Some(crate::grammar::TomlGrammar::try_new_direct(dialect).with_prune(prune)),
            builder: None,
            root: None,
            binding_stage: None,
            bound: false,
            finalizer: None,
            published: false,
            product: None,
        }
    }
}

impl AccessSession for TomlParseState {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
    fn decode<'source>(
        &mut self,
        input: AccessInput<'_, 'source>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        let source = match input {
            AccessInput::Source(source) => source,
            AccessInput::Document(_) => {
                return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
            }
        };
        if self.published {
            return Err(data_contract());
        }
        loop {
            match self.phase {
                Phase::Parse => {
                    // Drive the resumable grammar one statement per admitted work quantum; a long document yields
                    // across polls instead of blocking one.
                    let grammar = self.grammar.as_mut().ok_or_else(data_contract)?;
                    match grammar.poll(source, context.resources())? {
                        crate::grammar::GrammarPoll::Pending => {
                            context.replenish_work()?;
                        }
                        crate::grammar::GrammarPoll::ReadyDoc(doc) => {
                            self.grammar = None;
                            // The parse-DIRECT build: the document is built from the flat table state without the
                            // intermediate tree.
                            let (builder, root, spans_committed) = build_document_from_doc(
                                doc,
                                source.bytes().len(),
                                self.lazy_frontier,
                                self.dialect,
                                self.prune.as_ref(),
                                self.coverage,
                                context.resources(),
                            )?;
                            self.builder = Some(builder);
                            self.root = Some(root);
                            if spans_committed {
                                // The whole source IS the document segment: its seal is known from the first byte, and
                                // must cover every span the build admitted before the document may finish.
                                self.binding_stage = Some(DocumentSourceBindingStage::new(source).map_err(map_data)?);
                                self.phase = Phase::Seal;
                            } else {
                                self.begin_finalize(context.resources())?;
                            }
                        }
                        #[cfg(test)]
                        crate::grammar::GrammarPoll::Ready(_) => {
                            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                                contract: "the whole-document session is in parse-direct mode",
                            }));
                        }
                    }
                }
                Phase::Seal => {
                    let stage = self.binding_stage.as_mut().ok_or_else(data_contract)?;
                    // SAFETY: codec-core retains one immutable source authority
                    // for the complete access session and passes that exact
                    // authority each poll; the stage was constructed over the
                    // same segment and re-verifies identity on every call.
                    match unsafe { stage.poll(source, context.resources()) }.map_err(map_data)? {
                        jqf_data::DocumentSourceBindingPoll::Pending => {
                            context.replenish_work()?;
                        }
                        jqf_data::DocumentSourceBindingPoll::Ready(binding) => {
                            self.binding_stage = None;
                            self.builder
                                .as_mut()
                                .ok_or_else(data_contract)?
                                .bind_source(binding)
                                .map_err(map_data)?;
                            self.bound = true;
                            self.begin_finalize(context.resources())?;
                        }
                    }
                }
                Phase::Finalize => {
                    // The whole source is the sealed segment a binding names, so the source-backed finalization arm
                    // passes it whole.
                    let sealed = self.bound.then_some(source);
                    let finalizer = self.finalizer.as_mut().ok_or_else(data_contract)?;
                    let poll = if let Some(sealed) = sealed {
                        // SAFETY: the codec-core access session owns and
                        // supplies the same immutable ResolvedSource authority
                        // for every parser poll, and the whole source is the exact segment the binding was taken over.
                        unsafe { finalizer.poll_with_source(sealed, context.resources()) }.map_err(map_data)?
                    } else {
                        finalizer.poll(context.resources()).map_err(map_data)?
                    };
                    let DocumentFinalizationPoll::Ready(document) = poll else {
                        context.replenish_work()?;
                        continue;
                    };
                    self.finalizer = None;
                    self.product = Some(DocumentProduct::try_new(document, context.resources())?);
                    self.phase = Phase::Publish;
                }
                Phase::Publish => {
                    let product = self.product.take().ok_or_else(data_contract)?;
                    // Dark-launch instrumentation, taken here rather than at each commit so a declined-and-re-decoded
                    // document does not leave the abandoned attempt's spans in the count.
                    jqf_codec_core::record_published_spans(u64::from(product.document().container_span_count()));
                    let product = if self.bound {
                        // SAFETY: codec-core owns this exact immutable ResolvedSource for the whole access session; the
                        // binding was taken over this same whole segment.
                        unsafe { product.attach_borrowed_source_from_access_session(source, context.resources()) }?
                    } else {
                        product
                    };
                    self.published = true;
                    return Ok(AccessResult::from_outcome(AccessOutcome::FullDocument(product)));
                }
            }
        }
    }
}

impl TomlParseState {
    fn begin_finalize(&mut self, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        let root = self.root.take().ok_or_else(data_contract)?;
        let builder = self.builder.take().ok_or_else(data_contract)?;
        self.finalizer = Some(builder.begin_finish(root, resources).map_err(map_data)?);
        self.phase = Phase::Finalize;
        Ok(())
    }
}

pub(crate) fn data_contract() -> CodecError {
    jqf_codec_core::data_contract("TOML authoritative document construction")
}

/// Parses one complete buffer cooperatively in parse-DIRECT mode, handing back the flat table state. Every lazy
/// re-parse and the scoped empty-path route share this entry point instead of the tree-mode one-shot wrapper.
pub(crate) fn parse_direct(
    source: jqf_source::ResolvedSource<'_>,
    dialect: DialectKind,
    resources: &mut ResourceContext<'_>,
) -> Result<crate::grammar::Doc, CodecError> {
    let mut grammar = crate::grammar::TomlGrammar::try_new_direct(dialect);
    loop {
        match grammar.poll(source, resources)? {
            crate::grammar::GrammarPoll::Pending => {
                resources.try_begin_next_cooperative_entry(4_096)?;
            }
            crate::grammar::GrammarPoll::ReadyDoc(doc) => return Ok(doc),
            #[cfg(test)]
            crate::grammar::GrammarPoll::Ready(_) => {
                return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "parse-direct grammar must hand over the doc",
                }));
            }
        }
    }
}

/// The root table's sole assignment value after a one-assignment synthetic document re-parse (`x = <region>`).
pub(crate) fn first_assignment_value(doc: &crate::grammar::Doc) -> Result<&crate::grammar::Tree, CodecError> {
    doc.table(&crate::grammar::Path::default())
        .assignments
        .first()
        .map(|(_, value)| value)
        .ok_or_else(data_contract)
}

/// Id-walking target for the scoped table answer (no `TableTree` clone).
pub(crate) enum WalkTarget {
    Table(u32),
    ArrayOfTables(alloc::vec::Vec<u32>),
}

pub(crate) fn target_ids_for_walk(
    doc: &crate::grammar::Doc,
    key_depth: usize,
    element: bool,
) -> Result<WalkTarget, CodecError> {
    use crate::grammar::{Child, Path};
    if key_depth == 0 {
        return Err(data_contract());
    }
    let mut table_id = 0u32;
    let mut path = Path::default();
    let mut remaining = key_depth;
    loop {
        let part = doc.table(&path).child_order.first().ok_or_else(data_contract)?.id;
        path = path.push_key_id(part);
        remaining -= 1;
        match doc.child(table_id, part) {
            Some(Child::Table(child_id)) => {
                if remaining == 0 {
                    if element {
                        return Err(data_contract());
                    }
                    return Ok(WalkTarget::Table(child_id));
                }
                table_id = child_id;
            }
            Some(Child::Array { id, .. }) => {
                if remaining > 0 {
                    let elements = doc.array_elements(id).ok_or_else(data_contract)?;
                    if elements.len() != 1 {
                        return Err(data_contract());
                    }
                    table_id = elements[0];
                    path = path.push_elem(0);
                } else if element {
                    let elements = doc.array_elements(id).ok_or_else(data_contract)?;
                    if elements.len() != 1 {
                        return Err(data_contract());
                    }
                    return Ok(WalkTarget::Table(elements[0]));
                } else {
                    let elements = doc.array_elements(id).ok_or_else(data_contract)?.to_vec();
                    return Ok(WalkTarget::ArrayOfTables(elements));
                }
            }
            None => return Err(data_contract()),
        }
    }
}

/// Moves the target subtree out of the Doc (no Key/Tree clone) and builds the copy-mode located document from it.
pub(crate) fn build_located_from_doc(
    doc: &mut crate::grammar::Doc,
    target: &WalkTarget,
    bytes: &[u8],
    coverage: BuilderCoverage,
    resources: &ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    match target {
        WalkTarget::Table(id) => {
            // The located tree is a bare TableTree: its header/foot comment runs live in the flat TableData beside it,
            // so they are taken out here and attached on the built root node — matching the whole route, which
            // attaches them inside its own table build. Deeper tables inside the subtree keep their narrowing.
            let (header_comments, foot_comments) = {
                let data = doc.table_data_mut(*id);
                (
                    core::mem::take(&mut data.header_comments),
                    core::mem::take(&mut data.foot_comments),
                )
            };
            let table = doc.take_subtree(*id);
            let (mut builder, root) = crate::materialize::build_located_document(
                &crate::locate::Located::Table(&table),
                doc.names(),
                bytes,
                coverage,
                resources,
            )?;
            crate::parse::attach_comments(
                &mut builder,
                &header_comments,
                &[],
                root,
                coverage.attached_facts(),
                resources,
            )?;
            crate::parse::attach_foot_comments(
                &mut builder,
                &foot_comments,
                root,
                coverage.attached_facts(),
                resources,
            )?;
            Ok((builder, root))
        }
        WalkTarget::ArrayOfTables(ids) => {
            let mut tables = alloc::vec::Vec::with_capacity(ids.len());
            for id in ids {
                tables.push(doc.take_subtree(*id));
            }
            crate::materialize::build_located_document(
                &crate::locate::Located::ArrayOfTables(&tables),
                doc.names(),
                bytes,
                coverage,
                resources,
            )
        }
    }
}

/// The single element table after re-parsing one array-of-tables element's statement spans.
pub(crate) fn single_aot_element_subtree(
    doc: &mut crate::grammar::Doc,
) -> Result<crate::grammar::TableTree, CodecError> {
    use crate::grammar::{Child, Path};
    let part = doc
        .table(&Path::default())
        .child_order
        .first()
        .ok_or_else(data_contract)?
        .id;
    let base = Path::default().push_key_id(part);
    let Child::Array { id, .. } = doc.child(0, part).ok_or_else(data_contract)? else {
        return Err(data_contract());
    };
    let elements = doc.array_elements(id).ok_or_else(data_contract)?;
    if elements.len() != 1 {
        return Err(data_contract());
    }
    Ok(doc.subtree(&base.push_elem(0)))
}

/// Builds the semantic document from the parsed table tree.
///
/// Each document gets a FRESH schema from the validated recipe via `try_new_prepared_with_coverage` (not the shared
/// prototype builder, whose shared-schema builder refuses the dynamic `add_node` path): this lets the builder use
/// ordinary dynamic `add_node`/`add_occurrence` for every TOML semantic — including the temporal kinds, which have no
/// prepared handle. The schema is request-accounted like any other arena.
///
/// The returned `spans_committed` flag tells the caller whether the build admitted any source span, which decides
/// whether the source must be sealed and bound before the document can finish.
fn build_document_from_doc(
    doc: crate::grammar::Doc,
    source_len: usize,
    frontier: Option<usize>,
    dialect: DialectKind,
    prune: Option<&PruneLookup>,
    coverage: BuilderCoverage,
    resources: &ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId, bool), CodecError> {
    let (mut builder, schema) = fresh_builder(coverage, resources)?;
    // The container-span frontier: a forced frontier binds the dialect's span materializer, so a deferred value region
    // re-parses under the SAME grammar that validated it.
    if frontier.is_some() {
        builder.bind_span_materializer(match dialect {
            DialectKind::Toml10 => &crate::lazy::TOML_10_SPAN_MATERIALIZER,
            DialectKind::Toml11 => &crate::lazy::TOML_11_SPAN_MATERIALIZER,
        });
    }
    reserve_estimate(&mut builder, source_len, resources);
    let root = add_prepared_table(&mut builder, &schema, resources)?;
    let mut spans_committed = false;
    let mut doc = doc;
    let trailer_comments = core::mem::take(&mut doc.trailer_comments);
    // The prune hint rides the root: which top-level members the requesting program provably reads, propagated down
    // through child tables and arrays of tables. The grammar parse has ALREADY validated every key and value, so
    // omitting an unread member's build changes nothing observable.
    let root_prune = PruneRef::root(prune);
    build_table_from_doc(
        &mut builder,
        &schema,
        &mut doc,
        0,
        root,
        frontier,
        root_prune,
        0,
        coverage,
        &mut spans_committed,
        resources,
    )?;
    attach_comments(
        &mut builder,
        &trailer_comments,
        &[],
        root,
        coverage.attached_facts(),
        resources,
    )?;
    Ok((builder, root, spans_committed))
}

/// Builds one table's members DIRECTLY from the flat table state, consuming its assignments and child keys instead of
/// assembling the intermediate tree. The emission order is exactly the tree route's — a table's assignments in
/// authored order, then its children in first-definition order — which the statement stream already produces (a
/// table's own assignments are contiguous until a header moves the current table; children appear in first-definition
/// order).
#[expect(
    clippy::too_many_lines,
    reason = "one table-body walk: assignments plus child tables/arrays of tables, with the frontier depth threaded beside the span ledger"
)]
#[expect(
    clippy::too_many_arguments,
    reason = "one flat-state table walk: the builder, schema, flat state, path, owner, and the span/frontier ledger are the whole shape"
)]
fn build_table_from_doc(
    builder: &mut AccountedDocumentBuilder<'_>,
    schema: &PreparedDocumentSchema,
    doc: &mut crate::grammar::Doc,
    table_id: u32,
    owner: NodeId,
    frontier: Option<usize>,
    prune: PruneRef<'_>,
    depth: usize,
    coverage: BuilderCoverage,
    spans_committed: &mut bool,
    resources: &ResourceContext<'_>,
) -> Result<(), CodecError> {
    let member_role = schema.occurrence_role(0).ok_or_else(data_contract)?;
    // Direct assignments in authored order, moved out of the flat state.
    let (assignments, header_comments, foot_comments, children) = {
        let data = doc.table_data_mut(table_id);
        (
            core::mem::take(&mut data.assignments),
            core::mem::take(&mut data.header_comments),
            core::mem::take(&mut data.foot_comments),
            core::mem::take(&mut data.child_order),
        )
    };
    // The table's OWN header line binds the table node's span (the edit lane's structural splice reads it to place a
    // new member of an EMPTY section — a table with no direct statements has no other anchor). The root table and
    // dotted-key implicit tables have no header and bind nothing.
    let header_span = {
        let data = doc.table_data_mut(table_id);
        data.header_span
    };
    record_authored_span(builder, spans_committed, owner, header_span, resources)?;
    for (key, value) in assignments {
        // The prune hint names the members the program provably reads; an unobservable assignment is OMITTED (its
        // grammar validation already ran during the parse).
        let value_prune = prune.member(doc.name_text(key.id).as_bytes()).unwrap_or(PRUNE_ALL);
        let node = build_value(
            builder,
            schema,
            &value,
            doc.names(),
            frontier,
            prune.at(value_prune),
            depth + 1,
            coverage,
            spans_committed,
            resources,
        )?;
        add_member_occurrence(
            builder,
            schema,
            TABLE_ROLE,
            member_role,
            owner,
            &key,
            doc.name_text(key.id),
            node,
            spans_committed,
            resources,
        )?;
    }
    attach_comments(
        builder,
        &header_comments,
        &[],
        owner,
        coverage.attached_facts(),
        resources,
    )?;
    attach_foot_comments(builder, &foot_comments, owner, coverage.attached_facts(), resources)?;
    // Child tables and arrays of tables, in first-definition order.
    for key in children {
        // An unobservable child is OMITTED wholesale — its subtree holds no member the program reads. The spine law
        // still holds for the PARENT (this table's own node was built); an omitted child is simply a member that never
        // exists in the pruned document.
        let child_prune = prune.member(doc.name_text(key.id).as_bytes()).unwrap_or(PRUNE_ALL);
        let part = key.id;
        match doc.child(table_id, part) {
            // An array-of-tables child: its element ids are the flat state's ledger, walked by id instead of resolving
            // each element path.
            Some(crate::grammar::Child::Array { id: array_id, .. }) => {
                let array_node = add_prepared_array(builder, schema, resources)?;
                // The tree names the ARRAY position's demand; each table element prunes through its shared element
                // node.
                let element_prune = prune.at(child_prune).element();
                let element_ids = doc.take_array_elements(array_id);
                for element_id in element_ids {
                    let element_node = add_prepared_table(builder, schema, resources)?;
                    build_table_from_doc(
                        builder,
                        schema,
                        doc,
                        element_id,
                        element_node,
                        frontier,
                        element_prune,
                        depth + 1,
                        coverage,
                        spans_committed,
                        resources,
                    )?;
                    builder
                        .add_occurrence(
                            jqf_data::LocalOwnerRef::Node(array_node),
                            ARRAY_ROLE,
                            None,
                            element_node,
                            resources,
                        )
                        .map_err(map_data)?;
                }
                add_member_occurrence(
                    builder,
                    schema,
                    TABLE_ROLE,
                    member_role,
                    owner,
                    &key,
                    doc.name_text(key.id),
                    array_node,
                    spans_committed,
                    resources,
                )?;
            }
            // A plain child table.
            Some(crate::grammar::Child::Table(child_id)) => {
                let child_node = add_prepared_table(builder, schema, resources)?;
                build_table_from_doc(
                    builder,
                    schema,
                    doc,
                    child_id,
                    child_node,
                    frontier,
                    prune.at(child_prune),
                    depth + 1,
                    coverage,
                    spans_committed,
                    resources,
                )?;
                add_member_occurrence(
                    builder,
                    schema,
                    TABLE_ROLE,
                    member_role,
                    owner,
                    &key,
                    doc.name_text(key.id),
                    child_node,
                    spans_committed,
                    resources,
                )?;
            }
            None => {
                return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "a child_order entry always has a children index entry",
                }));
            }
        }
    }
    Ok(())
}

/// Creates a fresh request-accounted TOML builder and its prepared schema. `coverage` is the bound requirement's
/// [`required_builder_coverage`](jqf_codec_core::required_builder_coverage): identity `.` skips comment facts;
/// `.@comment` and `FactIntent::Preserve` attach them. The scoped route shares this entry point.
pub(crate) fn fresh_builder(
    coverage: BuilderCoverage,
    _resources: &ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, PreparedDocumentSchema), CodecError> {
    let recipe = toml_schema_recipe().map_err(map_data)?;
    let (mut builder, schema) =
        AccountedDocumentBuilder::try_new_prepared_with_coverage(&recipe, coverage).map_err(map_data)?;
    builder.set_authoritative_empty_families(AuthoritativeEmptyFamilies::from_family(
        DocumentCapabilityFamily::Attributes,
    ));
    builder.set_diagnostic_coverage(DiagnosticCoverage::NotRequested);
    Ok((builder, schema))
}

fn add_prepared_table(
    builder: &mut AccountedDocumentBuilder<'_>,
    schema: &PreparedDocumentSchema,
    resources: &ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    let kind = schema.node_kind(0).ok_or_else(data_contract)?;
    let role = schema.occurrence_role(0).ok_or_else(data_contract)?;
    builder
        .add_prepared_node(schema, kind, PreparedSemanticNode::Object(role), resources)
        .map_err(map_data)
}

fn add_prepared_inline_table(
    builder: &mut AccountedDocumentBuilder<'_>,
    schema: &PreparedDocumentSchema,
    resources: &ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    let kind = schema.node_kind(1).ok_or_else(data_contract)?;
    let role = schema.occurrence_role(1).ok_or_else(data_contract)?;
    builder
        .add_prepared_node(schema, kind, PreparedSemanticNode::Object(role), resources)
        .map_err(map_data)
}

fn add_prepared_array(
    builder: &mut AccountedDocumentBuilder<'_>,
    schema: &PreparedDocumentSchema,
    resources: &ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    let kind = schema.node_kind(2).ok_or_else(data_contract)?;
    let role = schema.occurrence_role(2).ok_or_else(data_contract)?;
    builder
        .add_prepared_node(schema, kind, PreparedSemanticNode::Array(role), resources)
        .map_err(map_data)
}

/// Conservative-low divisor turning source length into an initial node/occurrence arena reservation for the
/// whole-document route.
///
/// Derived from this crate's fixture corpus's measured node densities. Choosing `1/18` (0.0556) sizes the slab at or
/// below every shape's true density, so no input over-reserves (which would inflate peak on sparse documents), while
/// the densest lanes still start from a real slab that collapses their from-empty doublings to a couple. Publication
/// compaction releases whatever slack remains.
const SOURCE_BYTES_PER_ARENA_SLOT_ESTIMATE: usize = 18;

/// Conservative-low divisor for a source long enough that its arena would otherwise grow through many amortized
/// doublings. The initial slab only has to carry the build's first phase; like JSON's sampled divisor this sizes the
/// slab smaller on large inputs, trading a couple of doublings for a buffer the build does not hold live for its whole
/// extent. The exact reprojection loop JSON runs mid-parse is not mirrored here: TOML's statement-level parse and
/// one-shot build reserve once, up front, and the RSS gate's TOML lanes pin the result.
const SOURCE_BYTES_PER_SAMPLED_ARENA_SLOT: usize = 32;

/// Smallest source length that uses the sampled (smaller) divisor; shorter documents are sized for the whole document.
const CAPACITY_REPROJECT_MIN_SOURCE: usize = 1 << 20;

/// Reserves a node/occurrence slab as an optimization hint only: if the account cannot afford the estimate the
/// reservation is rolled back and the build degrades to amortized-from-empty growth.
fn reserve_estimate(builder: &mut AccountedDocumentBuilder<'_>, source_len: usize, resources: &ResourceContext<'_>) {
    let slots = if source_len >= CAPACITY_REPROJECT_MIN_SOURCE {
        source_len / SOURCE_BYTES_PER_SAMPLED_ARENA_SLOT
    } else {
        source_len / SOURCE_BYTES_PER_ARENA_SLOT_ESTIMATE
    };
    if slots == 0 {
        return;
    }
    let _ = builder.try_reserve(
        jqf_data::DocumentCapacity {
            nodes: slots,
            occurrences: slots,
            ..jqf_data::DocumentCapacity::default()
        },
        resources,
    );
}

/// Adds one member occurrence whose key is either a prepared source span (a verbatim bare/literal/basic key) or
/// ordinary copied text. The span arm is the zero-copy route: the document names the key's source bytes instead of
/// staging a copy in the decoded-text arena.
#[allow(
    clippy::too_many_arguments,
    reason = "one key occurrence dispatch: the prepared span arm and the copied-text arm share the shape"
)]
fn add_member_occurrence(
    builder: &mut AccountedDocumentBuilder<'_>,
    schema: &PreparedDocumentSchema,
    role: &'static str,
    prepared_role: PreparedOccurrenceRole,
    owner: NodeId,
    key: &Key,
    key_text: &str,
    target: NodeId,
    spans_committed: &mut bool,
    resources: &ResourceContext<'_>,
) -> Result<(), CodecError> {
    match key.span {
        Some(span) => {
            *spans_committed = true;
            // SAFETY: the grammar validated this key's bytes against the same
            // immutable source authority this session retains through
            // publication; admission proves containment against the seal.
            unsafe {
                builder.add_prepared_bound_source_occurrence(
                    schema,
                    LocalOwnerRef::Node(owner),
                    prepared_role,
                    span,
                    target,
                    resources,
                )
            }
            .map_err(map_data)?;
        }
        None => {
            builder
                .add_occurrence(
                    LocalOwnerRef::Node(owner),
                    role,
                    Some(AccountedOccurrenceKey::Text(key_text)),
                    target,
                    resources,
                )
                .map_err(map_data)?;
        }
    }
    Ok(())
}

/// Attaches the TOML comment facts to one value node: the leading set as `toml.comment@1` and the own-line trailing set
/// as `toml.comment_inline@1`. Shared by the whole-document and located builders so `.key.@comment` and
/// `.key.@comment_inline` serve on either route. A set with no entries attaches nothing.
pub(crate) fn attach_comments(
    builder: &mut AccountedDocumentBuilder<'_>,
    leading: &[String],
    inline: &[String],
    owner: NodeId,
    attach_facts: bool,
    resources: &ResourceContext<'_>,
) -> Result<(), CodecError> {
    if !attach_facts {
        return Ok(());
    }
    attach_comment_fact(builder, COMMENT_FACT, leading, owner, resources)?;
    attach_comment_fact(builder, COMMENT_INLINE_FACT, inline, owner, resources)?;
    Ok(())
}

/// Attaches a section-foot comment run as `toml.comment_foot@1` on the preceding table's node.
pub(crate) fn attach_foot_comments(
    builder: &mut AccountedDocumentBuilder<'_>,
    foot: &[String],
    owner: NodeId,
    attach_facts: bool,
    resources: &ResourceContext<'_>,
) -> Result<(), CodecError> {
    if !attach_facts {
        return Ok(());
    }
    attach_comment_fact(builder, COMMENT_FOOT_FACT, foot, owner, resources)
}

fn attach_comment_fact(
    builder: &mut AccountedDocumentBuilder<'_>,
    role: &str,
    comments: &[String],
    owner: NodeId,
    resources: &ResourceContext<'_>,
) -> Result<(), CodecError> {
    if comments.is_empty() {
        return Ok(());
    }
    let payload = FactPayload::List(comments.iter().map(|text| FactPayload::Text(text.clone())).collect());
    builder
        .add_fact(LocalOwnerRef::Node(owner), role, role, 1, &payload, resources)
        .map_err(map_data)?;
    Ok(())
}

/// Records one authored source span on the builder when the span is present (a TOML float/decimal/bool token), so the
/// edit lane can address the authored bytes for verbatim echo and patching. The semantic is stored by `add_node`; the
/// span is an addressing channel, never a second value.
fn record_authored_span(
    builder: &mut AccountedDocumentBuilder<'_>,
    spans_committed: &mut bool,
    node: NodeId,
    span: Option<Span>,
    resources: &ResourceContext<'_>,
) -> Result<(), CodecError> {
    if let Some(span) = span {
        // An out-of-band authored span is an admitted source span like any other: the seal binding is gated on
        // `spans_committed`, so recording one must set the flag or a document whose ONLY spans are out-of-band (a
        // float/bool-only TOML) would finalize without a seal covering them.
        *spans_committed = true;
        // SAFETY: the span was produced by this codec's own token walk over the
        // exact source authority bound to the builder, so it names UTF-8 that
        // re-resolves to the node's stored semantic — the `record_authored_span` contract.
        unsafe { builder.record_authored_span(node, span, resources) }.map_err(map_data)?;
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "one semantic construction dispatch: each TOML scalar and container kind is a few lines of the same shape, and splitting it would thread the builder through helpers"
)]
/// Builds one SCALAR node (a leaf: string, number, bool, temporal).
///
/// Kept OUT of [`build_value`] so the recursive container path never reserves the scalar arms' locals — a deep
/// document's per-level stack is the container path alone (the stack-depth gate's nesting lanes pin it).
fn build_scalar(
    builder: &mut AccountedDocumentBuilder<'_>,
    schema: &PreparedDocumentSchema,
    value: &Tree,
    spans_committed: &mut bool,
    resources: &ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    let scalar_kind = schema.node_kind(3).ok_or_else(data_contract)?;
    match value {
        Tree::String(source) => match source {
            TextSource::Copied(text) => builder
                .add_node("toml.scalar@1", AccountedSemanticNode::String(text), None, resources)
                .map_err(map_data),
            TextSource::Span(span) => {
                *spans_committed = true;
                // SAFETY: the grammar proved the span's bytes are the decoded
                // text of a verbatim string inside the same immutable source
                // authority this session retains through publication; admission proves containment against the seal.
                unsafe { builder.add_prepared_bound_source_string_node(schema, scalar_kind, *span, resources) }
                    .map_err(map_data)
            }
        },
        Tree::Integer { value: number, span } => {
            if let Some(span) = span {
                *spans_committed = true;
                // SAFETY: `integer_verbatim_span` proved the span's bytes ARE
                // the canonical jqf integer spelling of `number` inside the
                // same immutable source authority this session retains through publication.
                unsafe { builder.add_prepared_bound_source_integer_node(schema, scalar_kind, *span, resources) }
                    .map_err(map_data)
            } else {
                let text = alloc::format!("{number}");
                let mut stage = builder.begin_text(resources).map_err(map_data)?;
                builder.append_text(&mut stage, &text, resources).map_err(map_data)?;
                let text_id = builder.finish_text(stage, resources).map_err(map_data)?;
                builder
                    .add_prepared_stored_integer_node(schema, scalar_kind, text_id, resources)
                    .map_err(map_data)
            }
        }
        Tree::Float(float, span) => {
            let id = builder
                .add_node("toml.scalar@1", AccountedSemanticNode::Float(*float), None, resources)
                .map_err(map_data)?;
            record_authored_span(builder, spans_committed, id, *span, resources)?;
            Ok(id)
        }
        Tree::Decimal(coefficient, scale, span) => {
            let id = builder
                .add_node(
                    "toml.scalar@1",
                    AccountedSemanticNode::Decimal {
                        coefficient,
                        scale: *scale,
                    },
                    None,
                    resources,
                )
                .map_err(map_data)?;
            record_authored_span(builder, spans_committed, id, *span, resources)?;
            Ok(id)
        }
        Tree::Bool(value, span) => {
            let id = builder
                .add_node("toml.scalar@1", AccountedSemanticNode::Bool(*value), None, resources)
                .map_err(map_data)?;
            record_authored_span(builder, spans_committed, id, *span, resources)?;
            Ok(id)
        }
        Tree::LocalDate(date, span) => {
            let id = builder
                .add_node(
                    "toml.scalar@1",
                    AccountedSemanticNode::LocalDate(*date),
                    None,
                    resources,
                )
                .map_err(map_data)?;
            record_authored_span(builder, spans_committed, id, *span, resources)?;
            Ok(id)
        }
        Tree::LocalTime(time, span) => {
            let id = builder
                .add_node(
                    "toml.scalar@1",
                    AccountedSemanticNode::LocalTime(time.as_ref()),
                    None,
                    resources,
                )
                .map_err(map_data)?;
            record_authored_span(builder, spans_committed, id, *span, resources)?;
            Ok(id)
        }
        Tree::LocalDateTime(datetime, span) => {
            let id = builder
                .add_node(
                    "toml.scalar@1",
                    AccountedSemanticNode::LocalDateTime(datetime.as_ref()),
                    None,
                    resources,
                )
                .map_err(map_data)?;
            record_authored_span(builder, spans_committed, id, *span, resources)?;
            Ok(id)
        }
        Tree::OffsetDateTime(datetime, span) => {
            let id = builder
                .add_node(
                    "toml.scalar@1",
                    AccountedSemanticNode::OffsetDateTime(datetime.as_ref()),
                    None,
                    resources,
                )
                .map_err(map_data)?;
            record_authored_span(builder, spans_committed, id, *span, resources)?;
            Ok(id)
        }
        // The dispatch in [`build_value`] keeps containers and comments out of this helper; these arms exist only so
        // the match is total over the tree vocabulary.
        Tree::Array { .. } | Tree::InlineTable { .. } | Tree::Commented { .. } => Err(data_contract()),
    }
}

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "one container construction dispatch; the scalar arms live in build_scalar, and splitting them would thread the builder through helpers"
)]
fn build_value(
    builder: &mut AccountedDocumentBuilder<'_>,
    schema: &PreparedDocumentSchema,
    value: &Tree,
    names: &[String],
    frontier: Option<usize>,
    prune: PruneRef<'_>,
    depth: usize,
    coverage: BuilderCoverage,
    spans_committed: &mut bool,
    resources: &ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    if !matches!(
        value,
        Tree::Array { .. } | Tree::InlineTable { .. } | Tree::Commented { .. }
    ) {
        return build_scalar(builder, schema, value, spans_committed, resources);
    }
    match value {
        Tree::Array { items, span } => {
            // The container-span frontier: at the forced depth an array is one contiguous value region, so it defers to
            // a span instead of building its subtree.
            if frontier.is_some_and(|threshold| depth >= threshold) {
                let kind = schema.node_kind(2).ok_or_else(data_contract)?;
                return defer_container(
                    builder,
                    schema,
                    kind,
                    *span,
                    jqf_data::ContainerSpanKind::Array,
                    spans_committed,
                    resources,
                );
            }
            let array_node = add_prepared_array(builder, schema, resources)?;
            // The array's own authored extent: the edit lane's structural splice reads it to grow a literal inline
            // array (insert before the closing `]`). An array-of-tables has no single value region and binds none.
            record_authored_span(builder, spans_committed, array_node, Some(*span), resources)?;
            // Arrays never omit elements; each item's subtree prunes through the position's shared element node.
            let item_prune = prune.element();
            for item in items {
                let item_node = build_value(
                    builder,
                    schema,
                    item,
                    names,
                    frontier,
                    item_prune,
                    depth + 1,
                    coverage,
                    spans_committed,
                    resources,
                )?;
                builder
                    .add_occurrence(
                        jqf_data::LocalOwnerRef::Node(array_node),
                        ARRAY_ROLE,
                        None,
                        item_node,
                        resources,
                    )
                    .map_err(map_data)?;
            }
            Ok(array_node)
        }
        Tree::InlineTable {
            entries,
            span,
            implicit,
        } => {
            // The container-span frontier's object arm: a LITERAL inline table is one contiguous value region. An
            // implicit table (a dotted key's synthesized ancestor) has no such region — no `{...}` delimiters exist
            // in source for it — so it always builds eagerly regardless of depth.
            if !*implicit && frontier.is_some_and(|threshold| depth >= threshold) {
                let kind = schema.node_kind(1).ok_or_else(data_contract)?;
                return defer_container(
                    builder,
                    schema,
                    kind,
                    *span,
                    jqf_data::ContainerSpanKind::Object,
                    spans_committed,
                    resources,
                );
            }
            let table_node = add_prepared_inline_table(builder, schema, resources)?;
            // The inline table's authored extent (its `{...}` region): the edit lane's structural splice reads it to
            // grow the table in place. An implicit dotted-key table binds none.
            if !*implicit {
                record_authored_span(builder, spans_committed, table_node, Some(*span), resources)?;
            }
            let inline_role = schema.occurrence_role(1).ok_or_else(data_contract)?;
            for (key, entry) in entries {
                // An inline-table member the program provably cannot read is omitted; the grammar's O(n^2) duplicate
                // check already validated every entry during the parse.
                let entry_prune = prune.member(names[key.id as usize].as_bytes()).unwrap_or(PRUNE_ALL);
                let entry_node = build_value(
                    builder,
                    schema,
                    entry,
                    names,
                    frontier,
                    prune.at(entry_prune),
                    depth + 1,
                    coverage,
                    spans_committed,
                    resources,
                )?;
                add_member_occurrence(
                    builder,
                    schema,
                    INLINE_ROLE,
                    inline_role,
                    table_node,
                    key,
                    &names[key.id as usize],
                    entry_node,
                    spans_committed,
                    resources,
                )?;
            }
            Ok(table_node)
        }
        Tree::Commented { value, leading, inline } => {
            let node = build_value(
                builder,
                schema,
                value,
                names,
                frontier,
                prune,
                depth,
                coverage,
                spans_committed,
                resources,
            )?;
            attach_comments(builder, leading, inline, node, coverage.attached_facts(), resources)?;
            Ok(node)
        }
        // The dispatch in [`build_value`] sends every scalar to [`build_scalar`]; these arms exist only so the
        // container match is total over the tree vocabulary.
        Tree::String(_)
        | Tree::Integer { .. }
        | Tree::Float(..)
        | Tree::Decimal(..)
        | Tree::Bool(..)
        | Tree::LocalDate(..)
        | Tree::LocalTime(..)
        | Tree::LocalDateTime(..)
        | Tree::OffsetDateTime(..) => Err(data_contract()),
    }
}

/// Commits one container value as a span of the sealed source: the node names the validated extent and the subtree's
/// nodes, occurrences, and relationship arenas are never built. The span is only ever committed under a bound
/// materializer; a toucher re-parses the region under the document's own dialect.
fn defer_container(
    builder: &mut AccountedDocumentBuilder<'_>,
    schema: &PreparedDocumentSchema,
    kind: jqf_data::PreparedNodeKind,
    span: jqf_source::Span,
    container: jqf_data::ContainerSpanKind,
    spans_committed: &mut bool,
    resources: &ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    *spans_committed = true;
    // SAFETY: the grammar proved the span is one complete, already-validated
    // container value inside the same immutable source authority this session
    // retains through publication; admission proves containment against the seal.
    unsafe { builder.add_prepared_bound_container_span_node(schema, kind, span, container, resources) }
        .map_err(map_data)
}

/// The nesting-depth law: the value grammar and the table path grammar share the request's nesting ceiling, exactly
/// like the JSON codec): a 1M-deep array used to abort with a stack overflow and a deep table chain used to hang in the
/// flat state's path bookkeeping.
#[cfg(test)]
mod nesting_guard_tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec::Vec;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceLimits, WorkMeter};
    use jqf_source::ResolvedSource;

    static CONTROL: ContinueControl = ContinueControl;

    /// A resource context whose nesting ceiling is 64 levels: the guard must reject at 65 with the same error the JSON
    /// codec raises, fast.
    fn limited_resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, 64)).expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
    }

    fn source(bytes: &[u8]) -> ResolvedSource<'_> {
        ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(94), jqf_source::SourceKind::Input),
            "nesting.toml",
            bytes,
            0,
        )
    }

    fn poll_to_end(
        grammar: &mut crate::grammar::TomlGrammar,
        bytes: &[u8],
        ctx: &mut ResourceContext<'static>,
    ) -> Result<(), CodecError> {
        loop {
            match grammar.poll(source(bytes), ctx)? {
                crate::grammar::GrammarPoll::Pending => {
                    ctx.try_begin_next_cooperative_entry(4_096).expect("resume");
                }
                crate::grammar::GrammarPoll::Ready(_) | crate::grammar::GrammarPoll::ReadyDoc(_) => return Ok(()),
            }
        }
    }

    fn assert_nesting_limit(err: &CodecError) {
        use jqf_codec_core::CodecFailureKind;
        let CodecFailureKind::Resource(jqf_resource::ResourceError::LimitExceeded {
            limit_kind: jqf_resource::ResourceLimit::NestingDepth,
            limit: 64,
            current: 64,
            requested_delta: 1,
        }) = err.kind()
        else {
            panic!("expected the nesting-limit resource error, got {err:?}");
        };
    }

    #[test]
    fn deep_array_is_rejected_at_the_ceiling_not_overflowed() {
        let mut ctx = limited_resources();
        let at_ceiling = alloc::format!("a = {}1{}\n", "[".repeat(64), "]".repeat(64));
        let mut grammar = crate::grammar::TomlGrammar::try_new_direct(DialectKind::Toml10);
        poll_to_end(&mut grammar, at_ceiling.as_bytes(), &mut ctx).expect("64 levels parse");
        let over = alloc::format!("a = {}1{}\n", "[".repeat(65), "]".repeat(65));
        let mut grammar = crate::grammar::TomlGrammar::try_new_direct(DialectKind::Toml10);
        let err = poll_to_end(&mut grammar, over.as_bytes(), &mut ctx).expect_err("65 levels reject");
        assert_nesting_limit(&err);
    }

    #[test]
    fn deep_inline_table_is_rejected_at_the_ceiling() {
        let mut ctx = limited_resources();
        let over = alloc::format!("a = {}1{}\n", "{ x = ".repeat(65), " }".repeat(65));
        let mut grammar = crate::grammar::TomlGrammar::try_new_direct(DialectKind::Toml10);
        let err = poll_to_end(&mut grammar, over.as_bytes(), &mut ctx).expect_err("65 levels reject");
        assert_nesting_limit(&err);
    }

    #[test]
    fn deep_header_is_rejected_before_any_table_state_work() {
        let mut ctx = limited_resources();
        let parts: Vec<String> = (0..65).map(|i| alloc::format!("k{i}")).collect();
        let over = alloc::format!("[{}]\nx = 1\n", parts.join("."));
        let mut grammar = crate::grammar::TomlGrammar::try_new_direct(DialectKind::Toml10);
        let err = poll_to_end(&mut grammar, over.as_bytes(), &mut ctx).expect_err("65 parts reject");
        assert_nesting_limit(&err);
        // The same header at the ceiling parses (a bare header is a complete document; an assignment under it would
        // land one level deeper and is the NEXT test's subject).
        let parts: Vec<String> = (0..64).map(|i| alloc::format!("k{i}")).collect();
        let at_ceiling = alloc::format!("[{}]\n", parts.join("."));
        let mut grammar = crate::grammar::TomlGrammar::try_new_direct(DialectKind::Toml10);
        poll_to_end(&mut grammar, at_ceiling.as_bytes(), &mut ctx).expect("64 parts parse");
    }

    #[test]
    fn assignment_landing_depth_counts_the_current_table() {
        let mut ctx = limited_resources();
        let parts: Vec<String> = (0..64).map(|i| alloc::format!("k{i}")).collect();
        // The header reaches the ceiling; the assignment's landing is one level deeper and must be rejected.
        let over = alloc::format!("[{}]\nx = 1\n", parts.join("."));
        let mut grammar = crate::grammar::TomlGrammar::try_new_direct(DialectKind::Toml10);
        let err = poll_to_end(&mut grammar, over.as_bytes(), &mut ctx).expect_err("landing rejects");
        assert_nesting_limit(&err);
        // A 63-part header leaves room for the assignment.
        let parts: Vec<String> = (0..63).map(|i| alloc::format!("k{i}")).collect();
        let at_ceiling = alloc::format!("[{}]\nx = 1\n", parts.join("."));
        let mut grammar = crate::grammar::TomlGrammar::try_new_direct(DialectKind::Toml10);
        poll_to_end(&mut grammar, at_ceiling.as_bytes(), &mut ctx).expect("63 + 1 parse");
    }

    #[test]
    fn the_walk_rejects_deep_paths_with_the_same_ceiling() {
        use jqf_source::{SourceId, SourceKind, SourceRef};
        let ctx = limited_resources();
        let parts: Vec<String> = (0..65).map(|i| alloc::format!("k{i}")).collect();
        let over = alloc::format!("[{}]\nx = 1\n", parts.join("."));
        let walker = crate::walk::Walker::try_new(source(over.as_bytes()), DialectKind::Toml10, &[], &ctx, true);
        let err = walker.walk().expect_err("the walk rejects deep headers");
        assert_nesting_limit(&err);
        let deep_array = alloc::format!("a = {}1{}\n", "[".repeat(65), "]".repeat(65));
        let walker = crate::walk::Walker::try_new(source(deep_array.as_bytes()), DialectKind::Toml10, &[], &ctx, true);
        let err = walker.walk().expect_err("the walk rejects deep arrays");
        assert_nesting_limit(&err);
        let _ = (SourceId::new(1), SourceKind::Input, SourceRef::new);
    }
}

#[cfg(test)]
mod builder_tests {
    use super::*;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceLimits, WorkMeter};
    use jqf_source::ResolvedSource;

    static CONTROL: ContinueControl = ContinueControl;

    fn source(bytes: &[u8]) -> ResolvedSource<'_> {
        ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(93), jqf_source::SourceKind::Input),
            "dbg.toml",
            bytes,
            0,
        )
    }

    #[test]
    fn cooperative_parse_yields_between_statements() {
        // A tiny per-entry budget admits exactly one statement per poll, so a multi-statement document must yield
        // across several polls.
        let mut ctx = ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(1).expect("one credit per entry"),
        )
        .expect("resources");
        let mut grammar = crate::grammar::TomlGrammar::try_new(DialectKind::Toml10);
        let input = b"a = 1\nb = 2\nc = 3\nd = 4\ne = 5\n";
        let mut polls = 0u32;
        let parsed = loop {
            polls += 1;
            match grammar.poll(source(input), &mut ctx).expect("poll") {
                crate::grammar::GrammarPoll::Pending => {
                    ctx.try_begin_next_cooperative_entry(1).expect("resume");
                }
                crate::grammar::GrammarPoll::Ready(parsed) => break parsed,
                crate::grammar::GrammarPoll::ReadyDoc(_) => {
                    unreachable!("the cooperative test uses tree mode")
                }
            }
        };
        assert!(
            polls >= 5,
            "a five-statement document with a one-statement budget must yield at least five times, got {polls}"
        );
        assert_eq!(parsed.root.assignments.len(), 5);
    }

    #[test]
    fn comment_fact_attaches_through_the_whole_document_session() {
        use alloc::borrow::ToOwned;
        use alloc::string::ToString;
        use alloc::vec;
        use alloc::vec::Vec;
        use jqf_codec_core::AccessSession;
        use jqf_data::{BatchLimit, FactPayloadView, ReaderPoll};

        let mut ctx = crate::test_support::resources();
        let bytes: &'static [u8] = b"# the title\ntitle = \"catalog\" # a note\n";
        let src = source(bytes);
        let mut session = TomlParseState::try_new(
            DialectKind::Toml10,
            None,
            None,
            BuilderCoverage::minimal_semantic().with_attached_facts(true),
        );
        let mut context = jqf_codec_core::CodecRunContext::new(&mut ctx);
        let result = session.decode(AccessInput::Source(src), &mut context).expect("decode");
        let product = match result.outcome() {
            AccessOutcome::FullDocument(product) => product.try_clone().expect("clone"),
            AccessOutcome::Located(_) => panic!("expected full document"),
        };
        let mut found = Vec::new();
        let mut inline = Vec::new();
        let limit = BatchLimit::new(usize::MAX).expect("limit");
        let mut reader = product.document().fact_reader(&mut ctx).expect("reader");
        loop {
            match reader.poll_batch(limit, &mut ctx).expect("poll") {
                ReaderPoll::Batch(batch) => {
                    for fact in batch.iter() {
                        let target = match fact.role().as_str() {
                            COMMENT_FACT => &mut found,
                            COMMENT_INLINE_FACT => &mut inline,
                            _ => continue,
                        };
                        if let FactPayloadView::List(list) = fact.payload() {
                            for item in list.iter() {
                                if let FactPayloadView::Text(t) = item {
                                    target.push(t.to_string());
                                }
                            }
                        }
                    }
                }
                ReaderPoll::Pending => {
                    ctx.try_begin_next_cooperative_entry(4_096).expect("resume");
                }
                ReaderPoll::End(_) => break,
            }
        }
        // The leading block and the own-line trailing comment attach as two facts.
        assert_eq!(found, vec!["the title".to_owned()]);
        assert_eq!(inline, vec!["a note".to_owned()]);
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::{COMMENT_FACT, COMMENT_INLINE_FACT};
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use jqf_codec_core::{
        AccessFootprint, AccessGuarantees, AccessOutcome, AccessRequirement, CodecDemand, CodecError, CodecFailureKind,
        CodecRunContext, DecodeRequest, DemandClause, DiagnosticPolicy, ExactPath, FactIntent, ValidationMode,
    };
    use jqf_data::{FactKindId, FactPayloadView, FactRoleId};
    use jqf_resource::ResourceContext;
    use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

    fn resources() -> ResourceContext<'static> {
        crate::test_support::resources()
    }

    fn source(bytes: &[u8]) -> ResolvedSource<'_> {
        ResolvedSource::new(
            SourceRef::new(SourceId::new(93), SourceKind::Input),
            "coverage.toml",
            bytes,
            0,
        )
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

    fn decode_requirement_product<'bytes>(
        bytes: &'bytes [u8],
        requirement: &AccessRequirement,
        resources: &mut ResourceContext<'_>,
    ) -> Result<jqf_codec_core::DocumentProduct<'bytes>, CodecError> {
        let registration = crate::registration_1_0().expect("registration");
        let dialect = jqf_data::DialectId::try_new(crate::TOML_JQF_1_0_DIALECT_ID).expect("dialect");
        let mut provider = registration.decoder().expect("decoder").create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &dialect,
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            resources,
        )?;
        let handle = provider.bind(requirement).expect("bind");
        let mut session = provider.open(&handle, resources)?;
        let mut context = CodecRunContext::new(resources);
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
    fn identity_demand_does_not_attach_comment_facts() {
        let mut resources = resources();
        let requirement = whole_requirement(CodecDemand::try_new(&resources), &resources);
        let product = decode_requirement_product(b"# lead a\na = 1\n", &requirement, &mut resources).expect("decode");
        assert!(
            comment_facts(&product, &["a"], COMMENT_FACT).is_empty(),
            "identity must skip comment facts"
        );
        assert!(comment_facts(&product, &["a"], COMMENT_INLINE_FACT).is_empty());
    }

    #[test]
    fn comment_clause_attaches_comment_facts() {
        let mut resources = resources();
        let requirement = whole_requirement(attached_fact_demand("comment", &resources), &resources);
        let product = decode_requirement_product(b"# lead a\na = 1\n", &requirement, &mut resources).expect("decode");
        assert_eq!(
            comment_facts(&product, &["a"], COMMENT_FACT),
            vec![String::from("lead a")]
        );
    }

    #[test]
    fn preserve_attaches_comment_facts() {
        let mut resources = resources();
        let requirement =
            whole_requirement(CodecDemand::try_new(&resources), &resources).with_fact_intent(FactIntent::Preserve);
        let product =
            decode_requirement_product(b"# lead a\na = 1 # note\n", &requirement, &mut resources).expect("decode");
        assert_eq!(
            comment_facts(&product, &["a"], COMMENT_FACT),
            vec![String::from("lead a")]
        );
        assert_eq!(
            comment_facts(&product, &["a"], COMMENT_INLINE_FACT),
            vec![String::from("note")]
        );
    }

    #[test]
    fn exact_identity_demand_skips_comment_facts() {
        let mut resources = resources();
        let empty = CodecDemand::try_new(&resources);
        let requirement = exact_member_requirement("a", empty, &resources);
        let product = decode_requirement_product(b"# lead a\na = 1\n", &requirement, &mut resources).expect("decode");
        assert!(
            comment_facts(&product, &[], COMMENT_FACT).is_empty(),
            "Exact identity must skip comment facts"
        );
    }

    #[test]
    fn exact_comment_clause_attaches_comment_facts() {
        let mut resources = resources();
        let requirement = exact_member_requirement("a", attached_fact_demand("comment", &resources), &resources);
        let product = decode_requirement_product(b"# lead a\na = 1\n", &requirement, &mut resources).expect("decode");
        assert_eq!(comment_facts(&product, &[], COMMENT_FACT), vec![String::from("lead a")]);
    }
}
