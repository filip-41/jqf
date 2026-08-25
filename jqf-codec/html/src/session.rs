//! The HTML whole-document access session.
//!
//! Applies the WHATWG encoding determination (BOM, the bounded meta charset prescan, then the deterministic
//! windows-1252 fallback), runs the tokenizer + tree builder COOPERATIVELY (one admission check per tokenizer step, so
//! a large document yields `Pending` across polls instead of monopolizing one), projects the recovered tree into the
//! semantic document, seals the retained source cooperatively, and publishes a single `FullDocument` outcome.

use jqf_codec_core::{
    AccessInput, AccessOutcome, AccessResult, AccessSession, CodecError, CodecFailureKind, CodecRunContext,
    DocumentProduct,
};
use jqf_data::{AccountedDocumentBuilder, AccountedDocumentFinalizer, DocumentFinalizationPoll, NodeId};
use jqf_source::ResolvedSource;

use crate::document;
use crate::tree::{TreeBuildPoll, TreeBuilder};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    /// Decoding the retained bytes to text (the WHATWG encoding determination).
    Decode,
    /// Tokenizing and tree-building the decoded text cooperatively.
    Parse,
    /// Sealing the retained source cooperatively after the build.
    Seal,
    Finalize,
    Done,
}

/// One HTML document served through the whole-document route.
pub(crate) struct HtmlSession {
    phase: Phase,
    /// The in-flight cooperative tokenize + tree construction.
    tree_build: Option<TreeBuilder>,
    builder: Option<AccountedDocumentBuilder<'static>>,
    root: Option<NodeId>,
    /// The in-flight cooperative source seal, started after the build (the same accounting law the json/toml/yaml
    /// sessions keep).
    binding_stage: Option<jqf_data::DocumentSourceBindingStage>,
    finalizer: Option<AccountedDocumentFinalizer<'static>>,
    /// Parse as a FRAGMENT under the fixed default context (the `html.fragment@1` registration) rather than a document.
    fragment: bool,
}

impl HtmlSession {
    pub(crate) fn new(_source: ResolvedSource<'_>, fragment: bool) -> Result<Self, CodecError> {
        Ok(Self {
            phase: Phase::Decode,
            tree_build: None,
            builder: None,
            root: None,
            binding_stage: None,
            finalizer: None,
            fragment,
        })
    }
}

impl AccessSession for HtmlSession {
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
                Phase::Decode => {
                    let text = crate::decode::determine_and_decode(source.bytes())?;
                    self.tree_build = Some(TreeBuilder::begin_cooperative(text, self.fragment));
                    self.phase = Phase::Parse;
                }
                Phase::Parse => {
                    let build = self.tree_build.as_mut().ok_or_else(data_contract)?;
                    match build.poll_cooperative(context.resources()).map_err(|error| {
                        #[cfg(jqf_trace)]
                        std::eprintln!("session: tree build failed: {error:?}");
                        error
                    })? {
                        TreeBuildPoll::Pending => {
                            context.replenish_work()?;
                        }
                        TreeBuildPoll::Ready(tree) => {
                            self.tree_build = None;
                            let (builder, root) =
                                document::build_document(&tree, context.resources()).map_err(|error| {
                                    #[cfg(jqf_trace)]
                                    std::eprintln!("session: build_document failed: {error:?}");
                                    error
                                })?;
                            self.builder = Some(builder);
                            self.root = Some(root);
                            // The document retains the source authority the source-echo encoder reads back: seal it
                            // cooperatively (hash off — every consumer reads through metadata-checked access).
                            self.binding_stage =
                                Some(jqf_data::DocumentSourceBindingStage::new(source).map_err(document::map_data)?);
                            self.phase = Phase::Seal;
                        }
                    }
                }
                Phase::Seal => {
                    let stage = self.binding_stage.as_mut().ok_or_else(data_contract)?;
                    // SAFETY: codec-core retains one immutable source authority
                    // for the complete access session and passes that exact authority each decode; the stage was
                    // constructed over the same segment and re-verifies identity on every call.
                    match unsafe { stage.poll(source, context.resources()) }.map_err(|error| {
                        #[cfg(jqf_trace)]
                        std::eprintln!("session: seal failed: {error:?}");
                        document::map_data(error)
                    })? {
                        jqf_data::DocumentSourceBindingPoll::Pending => {
                            context.replenish_work()?;
                        }
                        jqf_data::DocumentSourceBindingPoll::Ready(binding) => {
                            self.binding_stage = None;
                            let builder = self.builder.as_mut().ok_or_else(data_contract)?;
                            builder.bind_source(binding).map_err(|error| {
                                #[cfg(jqf_trace)]
                                std::eprintln!("session: bind_source failed: {error:?}");
                                document::map_data(error)
                            })?;
                            self.phase = Phase::Finalize;
                        }
                    }
                }
                Phase::Finalize => {
                    if self.finalizer.is_none() {
                        let root = self.root.take().ok_or_else(data_contract)?;
                        let builder = self.builder.take().ok_or_else(data_contract)?;
                        self.finalizer = Some(builder.begin_finish(root, context.resources()).map_err(|error| {
                            #[cfg(jqf_trace)]
                            std::eprintln!("session: begin_finish failed: {error:?}");
                            document::map_data(error)
                        })?);
                    }
                    let finalizer = self.finalizer.as_mut().ok_or_else(data_contract)?;
                    // SAFETY: codec-core retains one immutable source authority
                    // for the complete access session and passes that exact authority each decode.
                    let poll = unsafe { finalizer.poll_with_source(source, context.resources()) }.map_err(|error| {
                        #[cfg(jqf_trace)]
                        std::eprintln!("session: poll_with_source failed: {error:?}");
                        document::map_data(error)
                    })?;
                    let DocumentFinalizationPoll::Ready(document) = poll else {
                        context.replenish_work()?;
                        continue;
                    };
                    self.finalizer = None;
                    self.phase = Phase::Done;
                    let product = DocumentProduct::try_new(document, context.resources())?;
                    // SAFETY: forwarded unchanged from `poll_with_source`'s
                    // contract — codec-core holds the exact immutable authority live for the whole session.
                    let product =
                        unsafe { product.attach_borrowed_source_from_access_session(source, context.resources())? };
                    let outcome = AccessOutcome::FullDocument(product);
                    return Ok(AccessResult::from_outcome(outcome));
                }
                Phase::Done => return Err(data_contract()),
            }
        }
    }
}

pub(crate) fn data_contract() -> CodecError {
    jqf_codec_core::data_contract("HTML session state missing during poll")
}
