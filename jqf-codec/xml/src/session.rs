//! The XML whole-document access session.
//!
//! Drive order is Parse → Seal → Finalize → Done. The source authority
//! seals after the tree is built so the source profile can echo the original
//! bytes.

use jqf_codec_core::{
    AccessInput, AccessOutcome, AccessResult, AccessSession, CodecError, CodecFailureKind, CodecRunContext,
    DocumentProduct,
};
use jqf_data::{AccountedDocumentBuilder, AccountedDocumentFinalizer, DocumentFinalizationPoll, NodeId};
use jqf_source::ResolvedSource;

use crate::document;
use crate::parse::{ParseOutput, ParsePoll, XmlParseState};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Parse,
    Seal,
    Finalize,
    Done,
}

/// One XML document served through the whole-document route.
///
/// The parser validates the retained source cooperatively (a large document
/// yields `Pending` across session polls), builds the semantic document,
/// finalizes it, and publishes a single `FullDocument` outcome. There is no
/// adjacent-value stream here in v1 (a single XML document is one document);
/// a multi-document stream is not served.
pub(crate) struct XmlSession {
    phase: Phase,
    parse: Option<XmlParseState>,
    builder: Option<AccountedDocumentBuilder<'static>>,
    root: Option<NodeId>,
    /// The in-flight cooperative source seal, started after the build so a
    /// large document's seal never blocks one poll (the same accounting law
    /// the json/toml/yaml sessions keep).
    binding_stage: Option<jqf_data::DocumentSourceBindingStage>,
    finalizer: Option<AccountedDocumentFinalizer<'static>>,
    /// CONTENT facts are format facts the encoder does not read; skip them
    /// when the demand names no attached-fact clause.
    attach_content: bool,
}

impl XmlSession {
    pub(crate) fn new(source: ResolvedSource<'_>, measure: bool, attach_content: bool) -> Result<Self, CodecError> {
        // The count skeleton: validate everything, record the document
        // element's direct children, build no tree.
        let parse = if measure {
            XmlParseState::try_new_measure(source.bytes())?
        } else {
            XmlParseState::try_new(source.bytes())?
        };
        Ok(Self {
            phase: Phase::Parse,
            parse: Some(parse),
            builder: None,
            root: None,
            binding_stage: None,
            finalizer: None,
            attach_content,
        })
    }
}

impl AccessSession for XmlSession {
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
        if self.phase == Phase::Done {
            return Err(data_contract());
        }
        loop {
            match self.phase {
                Phase::Parse => {
                    let parse = self.parse.as_mut().ok_or_else(data_contract)?;
                    let output = match parse.poll(source.bytes(), context.resources())? {
                        ParsePoll::Pending => {
                            context.replenish_work()?;
                            continue;
                        }
                        ParsePoll::Ready(output) => output,
                    };
                    self.parse = None;
                    let (builder, root) = match output {
                        // The measure skeleton: a root element node plus one span
                        // per child element and one leaf per text/comment/PI child.
                        // Only the root element's NAME fact is carried (the child
                        // spans make the descendant content fact unknowable without
                        // parsing them); the count consumer reads the child count.
                        ParseOutput::Measure(children) => {
                            document::build_measure_document(children, context.resources())?
                        }
                        ParseOutput::Tree(tree) => document::build_document_with_content(
                            &tree,
                            context.resources(),
                            true,
                            self.attach_content,
                        )?,
                        ParseOutput::Located(_) => {
                            return Err(jqf_codec_core::data_contract(
                                "XML whole-document session received a locate parse",
                            ));
                        }
                    };
                    self.builder = Some(builder);
                    self.root = Some(root);
                    // The document retains the source authority the source-echo
                    // encoder reads back: seal it cooperatively (hash off —
                    // every consumer reads through metadata-checked access).
                    self.binding_stage =
                        Some(jqf_data::DocumentSourceBindingStage::new(source).map_err(document::map_data)?);
                    self.phase = Phase::Seal;
                }
                Phase::Seal => {
                    let stage = self.binding_stage.as_mut().ok_or_else(data_contract)?;
                    // SAFETY: codec-core retains one immutable source authority
                    // for the complete access session and passes that exact
                    // authority each decode; the stage was constructed over the
                    // same segment and re-verifies identity on every call.
                    match unsafe { stage.poll(source, context.resources()) }.map_err(document::map_data)? {
                        jqf_data::DocumentSourceBindingPoll::Pending => {
                            context.replenish_work()?;
                        }
                        jqf_data::DocumentSourceBindingPoll::Ready(binding) => {
                            self.binding_stage = None;
                            let builder = self.builder.as_mut().ok_or_else(data_contract)?;
                            builder.bind_source(binding).map_err(document::map_data)?;
                            self.phase = Phase::Finalize;
                        }
                    }
                }
                Phase::Finalize => {
                    if self.finalizer.is_none() {
                        let root = self.root.take().ok_or_else(data_contract)?;
                        let builder = self.builder.take().ok_or_else(data_contract)?;
                        self.finalizer = Some(
                            builder
                                .begin_finish(root, context.resources())
                                .map_err(document::map_data)?,
                        );
                    }
                    let finalizer = self.finalizer.as_mut().ok_or_else(data_contract)?;
                    // The accounted source finalization seals the retained
                    // authority the source-echo encoder reads back. SAFETY:
                    // codec-core retains one immutable source authority for
                    // the complete access session and passes that exact
                    // authority each decode (the SDK does, and the binding was
                    // sealed from the same source in the Parse phase).
                    let poll = unsafe { finalizer.poll_with_source(source, context.resources()) }
                        .map_err(document::map_data)?;
                    let DocumentFinalizationPoll::Ready(document) = poll else {
                        context.replenish_work()?;
                        continue;
                    };
                    self.finalizer = None;
                    self.phase = Phase::Done;
                    let product = DocumentProduct::try_new(document, context.resources())?;
                    // Attach the retained source authority so the source-echo
                    // encoder can read the original bytes back. SAFETY:
                    // forwarded unchanged from `poll_with_source`'s contract —
                    // codec-core holds the exact immutable authority live for
                    // the whole session.
                    let product =
                        unsafe { product.attach_borrowed_source_from_access_session(source, context.resources()) }?;
                    let outcome = AccessOutcome::FullDocument(product);
                    return Ok(AccessResult::from_outcome(outcome));
                }
                Phase::Done => return Err(data_contract()),
            }
        }
    }
}

pub(crate) fn data_contract() -> CodecError {
    jqf_codec_core::data_contract("XML session state missing during poll")
}

#[cfg(test)]
mod measure_session_tests {
    use super::*;
    use jqf_codec_core::{AccessInput, AccessOutcome, CodecRunContext};
    use jqf_resource::ResourceContext;

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
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
        .expect("resources")
    }

    #[test]
    fn measure_session_publishes_the_skeleton_document() {
        let input = b"<catalog><item id=\"0\"/><item id=\"1\"/></catalog>";
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(0), jqf_source::SourceKind::Input),
            "input",
            input,
            0,
        );
        let mut resources = resources();
        let demand = jqf_data::CountDemand {
            row: jqf_data::CountRow::Container,
            path: Vec::new(),
            range: None,
            probe: Vec::new(),
            filter: None,
        };
        let mut session = XmlSession::new(source, true, false).expect("session");
        let mut context = CodecRunContext::new(&mut resources);
        let outcome = session
            .decode(AccessInput::Source(source), &mut context)
            .expect("decode");
        let AccessOutcome::FullDocument(product) = outcome.outcome() else {
            panic!("expected full document");
        };
        let document = product.document();
        assert_eq!(document.container_span_count(), 2, "two deferred children");
        assert_eq!(
            document.count_children_demand(&demand, &mut resources).expect("count"),
            jqf_data::CountVerdict::Count(2),
            "root length is the child count"
        );
    }
}
