//! Scoped YAML access: validate the whole input through the ordinary scanner+parser into the graph, navigate the exact
//! path over the graph, and materialize ONLY the located subtree.
//!
//! The route runs in two phases inside one poll: validate+parse into the graph (the same grammar, the same schema, the
//! same errors as the whole-document floor), then navigate the exact path and build a fresh demand-scoped document from
//! the located subtree — retained memory is proportional to the selected subtree, not the whole input.
//!
//! The published [`AccessOutcome::Located`] carries the identical [`ExactSelectionRecord`] the
//! whole-decode-then-navigate path publishes. Both negative observations publish a null PRODUCT — there is no subtree
//! to materialize — but the RECORD is what decides the answer, and the two observations are not the same answer. A
//! `Missing` path is the floor's own `null`. A `TypeMismatch` is the floor's own RAISE, and it stays one only because
//! the record carries the real kind of the value the step was applied to: `null` is the one kind a member step may
//! index, so a mismatch reporting `Null` would read as legal and answer `null` where the floor raises `Cannot index
//! array with string`. See [`locate::Located`].

use alloc::boxed::Box;
use alloc::vec::Vec;

use jqf_codec_core::{
    AccessInput, AccessOutcome, AccessResult, AccessSession, CodecError, CodecFailureKind, CodecRunContext,
    DocumentProduct, ExactSelectionRecord, LocatedOutcome, PortableStep, SelectionOrigin,
};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::document::data_contract;
use crate::graph::YamlGraph;
use crate::locate::{self, Located, OwnedStep};
use crate::parse::YamlParser;
use crate::provider::DialectKind;

/// Native scoped session state stored in the core-owned tracked carrier.
pub(crate) struct NativeScopedSession {
    steps: Vec<OwnedStep>,
    origin: SelectionOrigin,
    dialect: DialectKind,
    coverage: jqf_data::BuilderCoverage,
    /// See [`crate::document::demanded_intrinsic`].
    want_tags: bool,
    /// Re-anchored kept-subtree prune over the located node. `None` keeps every member.
    prune: Option<crate::document::PruneLookup>,
    /// Kind-only subtree: empty array/object or a dummy scalar. The graph is still fully parsed.
    type_demand: bool,
    /// The resumable validate+parse (None once the graph is in hand).
    parse: Option<GraphParse>,
    /// The retained decoded-source descriptor, moved out of [`GraphParse`] when its graph is taken: post-parse
    /// diagnostics (locate, subtree build) still need the decoded/original byte map for translation.
    decoded: Option<crate::scan::DecodedSource>,
    /// Whether the locate+materialize poll already ran to completion.
    finished: bool,
}

impl NativeScopedSession {
    pub(crate) fn try_new(
        steps: &[PortableStep],
        origin: SelectionOrigin,
        dialect: DialectKind,
        coverage: jqf_data::BuilderCoverage,
        want_tags: bool,
        prune: Option<crate::document::PruneLookup>,
        type_demand: bool,
    ) -> Result<Self, CodecError> {
        Ok(Self {
            steps: locate::own_steps(steps)?,
            origin,
            dialect,
            coverage,
            want_tags,
            prune,
            type_demand,
            parse: None,
            decoded: None,
            finished: false,
        })
    }

    /// Translates one post-parse diagnostic from decoded to original coordinates, when a real decoded-source descriptor
    /// is retained.
    fn translate(&self, error: CodecError, source: ResolvedSource<'_>) -> CodecError {
        match &self.decoded {
            Some(decoded) => decoded.translate_error(error, source.bytes().len()),
            None => error,
        }
    }

    fn poll_scoped<'source>(
        &mut self,
        source: ResolvedSource<'source>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        if source.bytes().len() > u32::MAX as usize {
            return Err(CodecError::new(CodecFailureKind::Overflow));
        }
        if self.finished {
            return Err(data_contract());
        }
        // Validate-everything-first: the ordinary scanner+parser builds the complete graph (the identical grammar and
        // errors as the whole-document floor). The parse is a straight-line loop that replenishes its own cooperative
        // budget at Pending; the outer loop here re-polls IN PLACE — a recursion per replenishment would put a stack
        // frame on every cooperative yield and overflow a debug build over a long stream.
        if self.parse.is_none() {
            self.parse = Some(GraphParse::try_new(source, self.dialect, context.resources())?);
        }
        let (graph, consumed_offset, source_mapped) = loop {
            // The resources borrow is scoped to the poll: the Pending arm re-borrows the context for its own replenish
            // below.
            let poll = {
                let resources = context.resources();
                self.parse.as_mut().expect("parse present").poll(source, resources)
            }?;
            match poll {
                GraphParsePoll::Pending => {
                    context.replenish_work()?;
                }
                GraphParsePoll::Ready(graph) => {
                    let mut parse = self.parse.take().expect("parse present");
                    let consumed_offset = parse.consumed_offset(source);
                    let source_mapped = parse.maps_to_source();
                    self.decoded = Some(parse.take_decoded());
                    break (graph, consumed_offset, source_mapped);
                }
            }
        };
        let resources = context.resources();
        self.finished = true;
        let located = locate::locate(&graph, self.steps.as_slice(), source, self.dialect)
            .map_err(|error| self.translate(error, source))?;
        let (product, selection) = match &located {
            Located::Node(node) => {
                let (builder, root) = if self.type_demand {
                    crate::scoped_build::build_kind_only_document(
                        &graph,
                        *node,
                        source,
                        self.dialect,
                        self.coverage,
                        self.want_tags,
                        resources,
                    )
                } else {
                    crate::scoped_build::build_subtree_document(
                        &graph,
                        *node,
                        source,
                        self.dialect,
                        source_mapped,
                        self.coverage,
                        self.want_tags,
                        self.prune.as_ref(),
                        resources,
                    )
                }
                .map_err(|error| self.translate(error, source))?;
                let document = builder.finish(root, resources).map_err(crate::document::map_data)?;
                let product = DocumentProduct::try_new(document, resources)?;
                let selection = ExactSelectionRecord::Node {
                    node: product.document().root_handle(),
                    origin: self.origin,
                };
                (product, selection)
            }
            Located::Range { items } => {
                let (builder, root) = if self.type_demand {
                    crate::scoped_build::build_empty_array_document(self.coverage, resources)
                } else {
                    crate::scoped_build::build_range_document(
                        &graph,
                        items,
                        source,
                        self.dialect,
                        source_mapped,
                        self.coverage,
                        self.want_tags,
                        self.prune.as_ref(),
                        resources,
                    )
                }
                .map_err(|error| self.translate(error, source))?;
                let document = builder.finish(root, resources).map_err(crate::document::map_data)?;
                let product = DocumentProduct::try_new(document, resources)?;
                let selection = ExactSelectionRecord::Node {
                    node: product.document().root_handle(),
                    origin: self.origin,
                };
                (product, selection)
            }
            Located::Missing { step } => {
                let product = null_product(source, resources)?;
                let selection = ExactSelectionRecord::Missing {
                    step_index: *step,
                    origin: self.origin,
                };
                (product, selection)
            }
            Located::TypeMismatch { step, actual } => {
                let product = null_product(source, resources)?;
                let selection = ExactSelectionRecord::TypeMismatch {
                    step_index: *step,
                    actual_type: *actual,
                    origin: self.origin,
                    hint: None,
                };
                (product, selection)
            }
        };
        let outcome = LocatedOutcome::try_new(&product, selection)?;
        // One item per `---` unit: report where THIS document ended so the SDK's reopen-at-offset sequence drive can
        // walk a pushdown program (`.field`, …) over every document in a YAML stream, not just the first.
        let consumed = u64::try_from(consumed_offset).unwrap_or(u64::MAX);
        Ok(AccessResult::from_outcome_with_consumed_offset(
            AccessOutcome::Located(outcome),
            consumed,
        ))
    }
}

impl AccessSession for NativeScopedSession {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
    fn decode<'source>(
        &mut self,
        input: AccessInput<'_, 'source>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        let AccessInput::Source(source) = input else {
            return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
        };
        self.poll_scoped(source, context)
    }
}

/// A RESUNABLE validate+parse into the graph (the shared validation walk).
///
/// The parser's work meter yields `Pending` when a cooperative entry's credits are spent; the session polls this across
/// ITS own polls (the SDK replenishes credits between session polls via `resume`). Looping on `Pending` here would spin
/// forever once the document exceeds ~4k parser steps — the whole-document session returns `Pending` to its caller for
/// the same reason.
pub(crate) struct GraphParse {
    decoded: crate::scan::DecodedSource,
    scanner: crate::scan::Scanner,
    parser: YamlParser,
    /// The dialect the parser resolved under: the shared duplicate-key validation re-resolves each mapping key's schema
    /// through it.
    dialect: DialectKind,
    done: bool,
    /// The decoded-text offset where the located document ended, valid once `done` (see `poll`'s two break arms: a
    /// `StreamEnd` with no document at all consumes the whole decoded text; a `DocumentEnd` uses the parser's own
    /// boundary tracking, identical to the whole-document route's).
    boundary: usize,
}

/// One resumable-parse observation.
pub(crate) enum GraphParsePoll {
    /// The cooperative entry's work credits are spent; re-poll after the caller replenishes.
    Pending,
    /// The document's graph is complete.
    Ready(Box<YamlGraph>),
}

impl GraphParse {
    pub(crate) fn try_new(
        source: ResolvedSource<'_>,
        dialect: DialectKind,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        let decoded = crate::scan::DecodedSource::try_new(source, resources)?;
        let start = decoded.start_offset();
        let scanner = crate::scan::Scanner::try_new(start);
        let parser = YamlParser::try_new(dialect)?;
        Ok(Self {
            decoded,
            scanner,
            parser,
            dialect,
            done: false,
            boundary: 0,
        })
    }

    /// The consumed span's end, in ORIGINAL source bytes (mapped through the decoded/original offset table). Only
    /// meaningful once `poll` returns `Ready`.
    pub(crate) fn consumed_offset(&self, source: ResolvedSource<'_>) -> usize {
        self.decoded.original(self.boundary, source.bytes().len())
    }

    /// Whether the decoded text the graph spans address IS the source's own bytes (UTF-8 input). A decoded (UTF-16/32)
    /// source's graph spans are decoded-coordinate and must never be sliced against the source.
    pub(crate) fn maps_to_source(&self) -> bool {
        self.decoded.maps_to_source()
    }

    /// Moves the retained decoded-source descriptor out, leaving a plain identity descriptor behind. The session keeps
    /// it so diagnostics raised AFTER the graph is taken (locate, subtree build) can still be translated from decoded
    /// to original coordinates.
    pub(crate) fn take_decoded(&mut self) -> crate::scan::DecodedSource {
        core::mem::replace(&mut self.decoded, crate::scan::DecodedSource::detached_identity())
    }

    /// Drives the scanner+parser until the document completes or the work budget yields.
    pub(crate) fn poll(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<GraphParsePoll, CodecError> {
        let source_len = source.bytes().len();
        self.poll_inner(source, resources)
            .map_err(|error| self.decoded.translate_error(error, source_len))
    }

    fn poll_inner(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<GraphParsePoll, CodecError> {
        if self.done {
            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "YAML graph parse polled after completion",
            }));
        }
        let text = self.decoded.text(source);
        loop {
            match self.parser.poll(text, &mut self.scanner, source, resources)? {
                crate::parse::ParserPoll::Pending => return Ok(GraphParsePoll::Pending),
                crate::parse::ParserPoll::Event(event) => match event {
                    crate::parse::GraphEvent::StreamEnd => {
                        // No document boundary was ever crossed (a genuinely empty/comment-only stream): the located
                        // route's one document is the whole decoded text.
                        self.boundary = text.len();
                        break;
                    }
                    crate::parse::GraphEvent::DocumentEnd => {
                        // One `---` unit per session. The parser tracked where this document's consumed span ends.
                        self.boundary = self.parser.document_boundary();
                        break;
                    }
                    _ => {}
                },
            }
        }
        self.done = true;
        let graph = self.parser.take_graph();
        // The shared whole-graph key validation: the scoped route drives this parse, so the non-string-key law and
        // `yaml.key-equivalence@1` both run here — validate-everything-first, before any byte is published.
        crate::locate::validate_duplicate_keys(&graph, source, self.dialect, resources)?;
        Ok(GraphParsePoll::Ready(Box::new(graph)))
    }
}

/// The null product of a negative observation (a missing or mismatched path).
fn null_product(
    source: ResolvedSource<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<DocumentProduct<'static>, CodecError> {
    let _ = source;
    let (builder, root) = crate::scoped_build::build_null_document(resources)?;
    let document = builder.finish(root, resources).map_err(crate::document::map_data)?;
    DocumentProduct::try_new(document, resources)
}
