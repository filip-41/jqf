//! The Exact/Located route: the whole document is recovered (validate-everything-first — the tokenizer and tree
//! builder are a single pass for HTML), the exact path navigates the recovered tree, and a fresh demand-scoped document
//! is built from the located subtree. Exact prune trims unread child elements of that subtree at materialize time; it
//! never skips recover.
//!
//! The recover runs EAGERLY at session creation, outside any cooperative poll loop, and it is one uncheckpointed pass
//! (the lenient tokenizer has no admission points to poll): a large input monopolizes its quantum by design. This
//! matches the messagepack located walk's uncheckpointed shape; resumable recovery is the recorded owe for both, not an
//! oversight here.
//!
//! The published [`AccessOutcome::Located`] carries the identical [`ExactSelectionRecord`] grammar the floor's own
//! navigate path publishes: a Node, or a Missing / TypeMismatch negative observation with the step index and the
//! observed kind. A member that hits two or more children is a stream, not one Located document: the session declines
//! ([`CodecFailureKind::RequirementMismatch`]) so the binder's whole-document floor plus engine navigation produce the
//! items.

use alloc::vec::Vec;

use jqf_codec_core::{
    AccessInput, AccessOutcome, AccessResult, AccessSession, CodecError, CodecFailureKind, CodecRunContext,
    DocumentProduct, ExactSelectionRecord, LocatedOutcome, PortableStep, PruneLookup, SelectionOrigin,
};
use jqf_data::BuilderCoverage;

use crate::document;
use crate::locate::{self, Located, OwnedStep};

/// Native scoped session state stored in the session carrier.
pub(crate) struct NativeScopedSession {
    steps: Vec<OwnedStep>,
    origin: SelectionOrigin,
    /// Re-anchored kept-subtree prune over the located node. `None` keeps every child. See
    /// [`document::build_subtree_document`] for the prune-after-recover law.
    prune: Option<PruneLookup>,
    /// Same coverage Whole uses: names always attach; attrs/comments skip when
    /// the demand named neither.
    coverage: BuilderCoverage,
    /// The recovered tree (built in the first poll, released after).
    tree: Option<crate::tree::Tree>,
    finished: bool,
}

impl NativeScopedSession {
    pub(crate) fn try_new(
        source: jqf_source::ResolvedSource<'_>,
        steps: &[PortableStep],
        origin: SelectionOrigin,
        fragment: bool,
        prune: Option<PruneLookup>,
        coverage: BuilderCoverage,
    ) -> Result<Self, CodecError> {
        let text = crate::decode::determine_and_decode(source.bytes())?;
        let tree = if fragment {
            crate::tree::TreeBuilder::build_fragment(text, crate::FRAGMENT_DEFAULT_CONTEXT)
        } else {
            crate::tree::TreeBuilder::build(text)
        };
        Ok(Self {
            steps: locate::own_steps(steps)?,
            origin,
            prune,
            coverage,
            tree: Some(tree),
            finished: false,
        })
    }

    fn poll_scoped<'source>(
        &mut self,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        if self.finished {
            return Err(locate::data_contract("HTML scoped session already finished"));
        }
        let tree = self
            .tree
            .take()
            .ok_or_else(|| locate::data_contract("HTML scoped session tree"))?;
        self.finished = true;
        let located = locate::locate(&tree, self.steps.as_slice());
        let (product, selection) = match &located {
            Located::Element(_) | Located::Leaf { .. } => {
                let (builder, root) = document::build_subtree_document(
                    &tree,
                    &located,
                    self.prune.as_ref(),
                    self.coverage,
                    context.resources(),
                )?;
                let document = builder.finish(root, context.resources()).map_err(document::map_data)?;
                let product = DocumentProduct::try_new(document, context.resources())?;
                let selection = ExactSelectionRecord::Node {
                    node: product.document().root_handle(),
                    origin: self.origin,
                };
                (product, selection)
            }
            Located::Range { .. } => return Err(document::decline_located_range()),
            Located::Missing { step } => {
                let (builder, root) = document::build_null_document(context.resources())?;
                let document = builder.finish(root, context.resources()).map_err(document::map_data)?;
                let product = DocumentProduct::try_new(document, context.resources())?;
                let selection = ExactSelectionRecord::Missing {
                    step_index: *step,
                    origin: self.origin,
                };
                (product, selection)
            }
            Located::TypeMismatch { step, actual, hint } => {
                let (builder, root) = document::build_null_document(context.resources())?;
                let document = builder.finish(root, context.resources()).map_err(document::map_data)?;
                let product = DocumentProduct::try_new(document, context.resources())?;
                let selection = ExactSelectionRecord::TypeMismatch {
                    step_index: *step,
                    actual_type: *actual,
                    origin: self.origin,
                    hint: hint.clone(),
                };
                (product, selection)
            }
        };
        let outcome = LocatedOutcome::try_new(&product, selection)?;
        Ok(AccessResult::from_outcome(AccessOutcome::Located(outcome)))
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
        let AccessInput::Source(_) = input else {
            return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
        };
        self.poll_scoped(context)
    }
}
