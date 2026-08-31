//! Native Exact access: scan the whole input, locate Member/Index over the skeleton, materialize only the hit.
//!
//! The ordinary scanner runs first, so last-wins keys, duplicate-section failure, and a corrupt unread byte fail
//! Exact as Whole. Locate walks the skeleton without building unused keys or sections. The published product is a
//! subtree document whose root IS the selection ([`ExactSelectionRecord::Node`] with `node == product.root_handle()`).
//! INI has no array slice: a range step declines [`CodecFailureKind::RequirementMismatch`] after the scan.
//!
//! Sibling: [`crate::scan`], [`crate::materialize`], [`crate::provider`].

use alloc::vec::Vec;

use jqf_codec_core::{
    AccessInput, AccessOutcome, AccessResult, AccessSession, CodecError, CodecFailureKind, CodecRunContext,
    DocumentProduct, ExactSelectionRecord, LocatedOutcome, OwnedStep, PortableStep, SelectionOrigin, own_steps,
};
use jqf_data::{
    AccountedDocumentBuilder, AccountedDocumentFinalizer, BuilderCoverage, DocumentFinalizationPoll,
    DocumentSourceBindingPoll, DocumentSourceBindingStage, NodeId, ValueKind,
};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::materialize;
use crate::options::Grammar;
use crate::scan::{self, Skeleton};

/// Native scoped session stored in the core-owned tracked carrier.
pub(crate) struct NativeScopedSession {
    steps: Vec<OwnedStep>,
    origin: SelectionOrigin,
    grammar: Grammar,
    coverage: BuilderCoverage,
    phase: Phase,
    builder: Option<AccountedDocumentBuilder<'static>>,
    root: Option<NodeId>,
    binding_stage: Option<DocumentSourceBindingStage>,
    finalizer: Option<AccountedDocumentFinalizer<'static>>,
    product: Option<DocumentProduct<'static>>,
    pending: Option<PendingSelection>,
    attach_source: bool,
    published: bool,
}

enum Phase {
    Scan,
    Seal,
    Finalize,
    Publish,
}

enum PendingSelection {
    Node,
    Missing { step: usize },
    TypeMismatch { step: usize, actual: ValueKind },
}

#[derive(Clone, Copy)]
enum Located {
    Root,
    Entry { index: usize },
    Section { index: u32 },
    Missing { step: usize },
    TypeMismatch { step: usize, actual: ValueKind },
}

enum Cursor {
    Root,
    Section(u32),
    Entry(usize),
}

impl NativeScopedSession {
    pub(crate) fn try_new(
        steps: &[PortableStep],
        origin: SelectionOrigin,
        grammar: Grammar,
        coverage: BuilderCoverage,
    ) -> Result<Self, CodecError> {
        Ok(Self {
            steps: own_steps(steps)?,
            origin,
            grammar,
            coverage,
            phase: Phase::Scan,
            builder: None,
            root: None,
            binding_stage: None,
            finalizer: None,
            product: None,
            pending: None,
            attach_source: false,
            published: false,
        })
    }

    fn begin_finalize(&mut self, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        let root = self.root.take().ok_or_else(data_contract)?;
        let builder = self.builder.take().ok_or_else(data_contract)?;
        self.finalizer = Some(builder.begin_finish(root, resources).map_err(map_data)?);
        self.phase = Phase::Finalize;
        Ok(())
    }

    fn start_hit(
        &mut self,
        source: ResolvedSource<'_>,
        builder: AccountedDocumentBuilder<'static>,
        root: NodeId,
    ) -> Result<(), CodecError> {
        self.builder = Some(builder);
        self.root = Some(root);
        self.binding_stage = Some(DocumentSourceBindingStage::new(source).map_err(map_data)?);
        self.pending = Some(PendingSelection::Node);
        self.attach_source = true;
        self.phase = Phase::Seal;
        Ok(())
    }

    fn start_negative(
        &mut self,
        pending: PendingSelection,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let (builder, root) = materialize::build_null_document(self.grammar, resources)?;
        let document = builder.finish(root, resources).map_err(map_data)?;
        self.product = Some(DocumentProduct::try_new(document, resources)?);
        self.pending = Some(pending);
        self.attach_source = false;
        self.phase = Phase::Publish;
        Ok(())
    }

    fn apply_located(
        &mut self,
        source: ResolvedSource<'_>,
        skeleton: &Skeleton,
        located: Located,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        match located {
            Located::Root => {
                let (builder, root) = materialize::build_document(skeleton, self.grammar, self.coverage, resources)?;
                self.start_hit(source, builder, root)
            }
            Located::Entry { index } => {
                let (builder, root) = materialize::build_string_document(
                    &skeleton.entries[index],
                    self.grammar,
                    self.coverage,
                    resources,
                )?;
                self.start_hit(source, builder, root)
            }
            Located::Section { index } => {
                let (builder, root) =
                    materialize::build_section_document(skeleton, index, self.grammar, self.coverage, resources)?;
                self.start_hit(source, builder, root)
            }
            Located::Missing { step } => self.start_negative(PendingSelection::Missing { step }, resources),
            Located::TypeMismatch { step, actual } => {
                self.start_negative(PendingSelection::TypeMismatch { step, actual }, resources)
            }
        }
    }

    #[allow(
        unsafe_code,
        reason = "the source-binding API is unsafe by jqf-data's design; the codec-core session continuously owns the exact whole-source authority every call receives"
    )]
    fn publish_located<'source>(
        &mut self,
        source: ResolvedSource<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        let product = self.product.take().ok_or_else(data_contract)?;
        let product = if self.attach_source {
            // SAFETY: codec-core owns this exact immutable ResolvedSource
            // for the whole access session; the binding was taken over this same whole segment.
            unsafe { product.attach_borrowed_source_from_access_session(source, resources)? }
        } else {
            product
        };
        let pending = self.pending.take().ok_or_else(data_contract)?;
        let selection = match pending {
            PendingSelection::Node => ExactSelectionRecord::Node {
                node: product.document().root_handle(),
                origin: self.origin,
            },
            PendingSelection::Missing { step } => ExactSelectionRecord::Missing {
                step_index: step,
                origin: self.origin,
            },
            PendingSelection::TypeMismatch { step, actual } => ExactSelectionRecord::TypeMismatch {
                step_index: step,
                actual_type: actual,
                origin: self.origin,
                hint: None,
            },
        };
        self.published = true;
        let outcome = LocatedOutcome::try_new(&product, selection)?;
        Ok(AccessResult::from_outcome(AccessOutcome::Located(outcome)))
    }
}

impl AccessSession for NativeScopedSession {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    #[allow(
        unsafe_code,
        reason = "the source-binding and span-admission APIs are unsafe by jqf-data's design; the codec-core session continuously owns the exact whole-source authority every call receives"
    )]
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
                Phase::Scan => {
                    let skeleton = scan::scan(source, self.grammar, context)?;
                    let located = locate(&skeleton, self.steps.as_slice())?;
                    self.apply_located(source, &skeleton, located, context.resources())?;
                }
                Phase::Seal => {
                    let stage = self.binding_stage.as_mut().ok_or_else(data_contract)?;
                    // SAFETY: codec-core retains one immutable source authority
                    // for the complete access session and passes that exact authority each poll; the stage was
                    // constructed over the same segment and re-verifies identity on every call.
                    match unsafe { stage.poll(source, context.resources()) }.map_err(map_data)? {
                        DocumentSourceBindingPoll::Pending => context.replenish_work()?,
                        DocumentSourceBindingPoll::Ready(binding) => {
                            self.binding_stage = None;
                            self.builder
                                .as_mut()
                                .ok_or_else(data_contract)?
                                .bind_source(binding)
                                .map_err(map_data)?;
                            self.begin_finalize(context.resources())?;
                        }
                    }
                }
                Phase::Finalize => {
                    let finalizer = self.finalizer.as_mut().ok_or_else(data_contract)?;
                    // SAFETY: the codec-core access session owns and supplies
                    // the same immutable ResolvedSource authority for every poll, and the whole source is the exact
                    // segment the binding was taken over.
                    let poll = unsafe { finalizer.poll_with_source(source, context.resources()) }.map_err(map_data)?;
                    let DocumentFinalizationPoll::Ready(document) = poll else {
                        context.replenish_work()?;
                        continue;
                    };
                    self.finalizer = None;
                    self.product = Some(DocumentProduct::try_new(document, context.resources())?);
                    self.phase = Phase::Publish;
                }
                Phase::Publish => return self.publish_located(source, context.resources()),
            }
        }
    }
}

/// Walks the validated skeleton. A range step declines so the binder's whole-document floor serves it.
fn locate(skeleton: &Skeleton, steps: &[OwnedStep]) -> Result<Located, CodecError> {
    let mut cursor = Cursor::Root;
    for (step_index, step) in steps.iter().enumerate() {
        match (&cursor, step) {
            (Cursor::Entry(_), OwnedStep::Member(_) | OwnedStep::Index(_)) => {
                return Ok(Located::TypeMismatch {
                    step: step_index,
                    actual: ValueKind::String,
                });
            }
            (Cursor::Root | Cursor::Section(_), OwnedStep::Index(_)) => {
                return Ok(Located::TypeMismatch {
                    step: step_index,
                    actual: ValueKind::Object,
                });
            }
            (Cursor::Root, OwnedStep::Member(name)) => {
                if let Some(index) = last_entry(skeleton, None, name) {
                    cursor = Cursor::Entry(index);
                } else if let Some(index) = skeleton.sections.iter().position(|section| section.name == *name) {
                    let index = u32::try_from(index).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?;
                    cursor = Cursor::Section(index);
                } else {
                    return Ok(Located::Missing { step: step_index });
                }
            }
            (Cursor::Section(section), OwnedStep::Member(name)) => {
                if let Some(index) = last_entry(skeleton, Some(*section), name) {
                    cursor = Cursor::Entry(index);
                } else {
                    return Ok(Located::Missing { step: step_index });
                }
            }
            (_, OwnedStep::Range { .. }) => return Err(decline_located_range()),
        }
    }
    Ok(match cursor {
        Cursor::Root => Located::Root,
        Cursor::Section(index) => Located::Section { index },
        Cursor::Entry(index) => Located::Entry { index },
    })
}

fn last_entry(skeleton: &Skeleton, section: Option<u32>, name: &str) -> Option<usize> {
    skeleton
        .entries
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, entry)| (entry.section == section && entry.key == name).then_some(index))
}

fn decline_located_range() -> CodecError {
    CodecError::new(CodecFailureKind::RequirementMismatch)
}

fn map_data(error: jqf_data::DataError) -> CodecError {
    jqf_codec_core::map_data(error, "flat-config builder rejected document construction")
}

fn data_contract() -> CodecError {
    jqf_codec_core::data_contract("flat-config authoritative document construction")
}
