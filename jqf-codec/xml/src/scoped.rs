//! Scoped XML access: the Exact/Located demand route.
//!
//! Validates the whole input through the ordinary parser into the [`Tree`]
//! (the identical grammar and errors as the whole-document floor — the
//! validate-everything-first law), navigates the exact path over the tree,
//! and builds a fresh demand-scoped document from the located subtree.
//! Retained memory is proportional to the selected subtree, not the whole
//! input.
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
    DocumentProduct, ExactSelectionRecord, LocatedOutcome, PortableStep, SelectionOrigin,
};
use jqf_source::ResolvedSource;

use alloc::vec::Vec;

use crate::document;
use crate::locate::{self, OwnedStep};
use crate::parse::{ParseOutput, ParsePoll, XmlParseState};

/// Native scoped session state stored in the session carrier.
pub(crate) struct NativeScopedSession {
    steps: Vec<OwnedStep>,
    origin: SelectionOrigin,
    /// The resumable validate-only parse (parsed across polls).
    parse: Option<XmlParseState>,
    /// Whether the locate+materialize poll already ran to completion.
    finished: bool,
}

impl NativeScopedSession {
    pub(crate) fn try_new(
        source: ResolvedSource<'_>,
        steps: &[PortableStep],
        origin: SelectionOrigin,
    ) -> Result<Self, CodecError> {
        let owned = locate::own_steps(steps)?;
        let parse = XmlParseState::try_new_locate(source.bytes(), locate::copy_steps(&owned))?;
        Ok(Self {
            steps: owned,
            origin,
            parse: Some(parse),
            finished: false,
        })
    }

    fn poll_scoped<'source>(
        &mut self,
        bytes: &[u8],
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        if self.finished {
            return Err(jqf_codec_core::data_contract("XML scoped session already finished"));
        }
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
        let hit = deepen_hit(bytes, hit, self.steps.as_slice(), context.resources())?;
        let (product, selection) = match &hit {
            locate::LocatedHit::Element { .. } | locate::LocatedHit::Leaf { .. } => {
                let (builder, root) = document::build_from_hit(bytes, &hit, context.resources())?;
                let document = builder.finish(root, context.resources()).map_err(document::map_data)?;
                let product = DocumentProduct::try_new(document, context.resources())?;
                let selection = ExactSelectionRecord::Node {
                    node: product.document().root_handle(),
                    origin: self.origin,
                };
                (product, selection)
            }
            locate::LocatedHit::Range { .. } => {
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

/// When the first-step locate returns element extents and more steps remain,
/// re-parse each matched extent with the remaining path. Off-path siblings
/// are never rebuilt.
fn deepen_hit(
    bytes: &[u8],
    hit: locate::LocatedHit,
    steps: &[locate::OwnedStep],
    resources: &mut jqf_resource::ResourceContext<'_>,
) -> Result<locate::LocatedHit, CodecError> {
    if steps.len() <= 1 {
        return Ok(hit);
    }
    let rest = &steps[1..];
    match hit {
        locate::LocatedHit::Element { start, end } => {
            let inner = locate_span(&bytes[start..end], rest, resources)?;
            Ok(offset_hit(rebase_hit(inner, start), 1))
        }
        locate::LocatedHit::Range { children } => {
            let mut out = Vec::new();
            for child in children {
                out.push(offset_hit(deepen_hit(bytes, child, rest, resources)?, 1));
            }
            Ok(locate::LocatedHit::Range { children: out })
        }
        locate::LocatedHit::Leaf { .. } => Ok(locate::LocatedHit::TypeMismatch {
            step: 1,
            actual: jqf_data::ValueKind::String,
            hint: None,
        }),
        other => Ok(other),
    }
}

fn offset_hit(hit: locate::LocatedHit, add: usize) -> locate::LocatedHit {
    match hit {
        locate::LocatedHit::Missing { step } => locate::LocatedHit::Missing { step: step + add },
        locate::LocatedHit::TypeMismatch { step, actual, hint } => locate::LocatedHit::TypeMismatch {
            step: step + add,
            actual,
            hint,
        },
        locate::LocatedHit::Range { children } => locate::LocatedHit::Range {
            children: children.into_iter().map(|child| offset_hit(child, add)).collect(),
        },
        other => other,
    }
}

fn rebase_hit(hit: locate::LocatedHit, base: usize) -> locate::LocatedHit {
    match hit {
        locate::LocatedHit::Element { start, end } => locate::LocatedHit::Element {
            start: base + start,
            end: base + end,
        },
        locate::LocatedHit::Range { children } => locate::LocatedHit::Range {
            children: children.into_iter().map(|child| rebase_hit(child, base)).collect(),
        },
        other => other,
    }
}

fn locate_span(
    span: &[u8],
    steps: &[locate::OwnedStep],
    resources: &mut jqf_resource::ResourceContext<'_>,
) -> Result<locate::LocatedHit, CodecError> {
    let mut parse = XmlParseState::try_new_locate_nested(span, locate::copy_steps(steps))?;
    loop {
        match parse.poll(span, resources)? {
            ParsePoll::Pending => {
                resources.try_begin_next_cooperative_entry(1)?;
            }
            ParsePoll::Ready(ParseOutput::Located(hit)) => {
                return deepen_hit(span, hit, steps, resources);
            }
            ParsePoll::Ready(_) => {
                return Err(jqf_codec_core::data_contract(
                    "XML scoped span re-parse was not a locate parse",
                ));
            }
        }
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
        self.poll_scoped(source.bytes(), context)
    }
}
