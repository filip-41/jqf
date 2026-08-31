//! Scoped XML access: the Exact/Located demand route.
//!
//! Validate-everything-first still walks the whole input (the identical
//! grammar and errors as the whole-document floor). Locate retention does
//! not keep the whole tree: a stack of child ledgers along the remaining
//! path, plus the winning element's direct-child count, are recorded on that
//! pass. [`locate::apply_steps`] walks those ledgers — nested locate does
//! not re-parse extents. Count/element Exact publishes the winning span
//! ([`document::publish_located_skeleton`]); print without the hint still
//! rematerializes the final hit.
//!
//! The published [`AccessOutcome::Located`] carries the identical
//! [`ExactSelectionRecord`] the whole-decode-then-navigate path publishes;
//! negative observations publish a null product, exactly the floor's own
//! `null` for a missing or mismatched path. A member that hits two or more
//! children is a stream, not one Located document: the session declines
//! ([`CodecFailureKind::RequirementMismatch`]) so the binder's whole-document
//! floor plus engine navigation produce the items.

use jqf_codec_core::{
    AccessInput, AccessOutcome, AccessResult, AccessSession, CodecError, CodecFailureKind, CodecRunContext,
    DocumentProduct, ExactSelectionRecord, LocatedOutcome, PortableStep, PruneLookup, SelectionOrigin,
};
use jqf_source::ResolvedSource;

use crate::document;
use crate::locate;
use crate::parse::{ParseOutput, ParsePoll, XmlParseState};

/// Native scoped session state stored in the session carrier.
pub(crate) struct NativeScopedSession {
    origin: SelectionOrigin,
    /// The resumable validate-only parse (parsed across polls).
    parse: Option<XmlParseState>,
    /// Count/element Exact: publish the locate element span instead of re-parsing.
    skeleton: bool,
    /// Re-anchored kept-subtree prune over the located node. `None` keeps every child.
    prune: Option<PruneLookup>,
    /// Whether the locate+materialize poll already ran to completion.
    finished: bool,
}

impl NativeScopedSession {
    pub(crate) fn try_new(
        source: ResolvedSource<'_>,
        steps: &[PortableStep],
        origin: SelectionOrigin,
        skeleton: bool,
        prune: Option<PruneLookup>,
    ) -> Result<Self, CodecError> {
        let parse = XmlParseState::try_new_locate(source.bytes(), locate::own_steps(steps)?)?;
        Ok(Self {
            origin,
            parse: Some(parse),
            skeleton,
            prune,
            finished: false,
        })
    }

    fn poll_scoped<'source>(
        &mut self,
        source: ResolvedSource<'source>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        if self.finished {
            return Err(jqf_codec_core::data_contract("XML scoped session already finished"));
        }
        let bytes = source.bytes();
        // Validate-everything-first, cooperatively: drive the parse to
        // completion, replenishing the budget at Pending.
        let parse = self
            .parse
            .as_mut()
            .ok_or_else(|| jqf_codec_core::data_contract("XML scoped session parse"))?;
        let hit = loop {
            match parse.poll(bytes, context.resources())? {
                ParsePoll::Pending => context.replenish_work()?,
                ParsePoll::Ready(ParseOutput::Located(hit)) => break hit,
                ParsePoll::Ready(_) => {
                    return Err(jqf_codec_core::data_contract(
                        "XML scoped session received a non-locate parse",
                    ));
                }
            }
        };
        self.parse = None;
        self.finished = true;
        let (product, selection) = match &hit {
            locate::LocatedHit::Element {
                start,
                end,
                child_count,
            } if self.skeleton => {
                let product = document::publish_located_skeleton(source, *start, *end, *child_count, context)?;
                let selection = ExactSelectionRecord::Node {
                    node: product.document().root_handle(),
                    origin: self.origin,
                };
                (product, selection)
            }
            locate::LocatedHit::Element { .. } | locate::LocatedHit::Leaf { .. } => {
                let (builder, root) = document::build_from_hit(bytes, &hit, context.resources(), self.prune.as_ref())?;
                let document = builder.finish(root, context.resources()).map_err(document::map_data)?;
                let product = DocumentProduct::try_new(document, context.resources())?;
                let selection = ExactSelectionRecord::Node {
                    node: product.document().root_handle(),
                    origin: self.origin,
                };
                (product, selection)
            }
            locate::LocatedHit::Range => {
                return Err(document::decline_located_range());
            }
            locate::LocatedHit::Missing { step } => {
                let (builder, root) = document::build_null_document(context.resources())?;
                let document = builder.finish(root, context.resources()).map_err(document::map_data)?;
                let product = DocumentProduct::try_new(document, context.resources())?;
                let selection = ExactSelectionRecord::Missing {
                    step_index: *step,
                    origin: self.origin,
                };
                (product, selection)
            }
            locate::LocatedHit::TypeMismatch { step, actual, hint } => {
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
        let AccessInput::Source(source) = input else {
            return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
        };
        self.poll_scoped(source, context)
    }
}
