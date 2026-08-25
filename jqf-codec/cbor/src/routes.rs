//! CBOR specialized access route (located).
//!
//! The route opens with the SAME whole-input validation pass as the whole-document floor — the byte WALK
//! ([`crate::walk::locate`]) validates the complete input to the generic dialect's exact strictness, so a corrupt byte
//! anywhere fails this route exactly as it fails the floor (the validate-everything-first law). The walk resolves the
//! target path over the WIRE without building any nodes, so retention is bounded to what the route actually
//! materializes: the located span's re-decoded subtree.
//!
//! The "second read is budgeted" law: the walk validated everything, so the span re-decode
//! ([`crate::parse::decode_span`] and the per-value descent it drives) assumes validity and materializes only the
//! located subtree. Navigation is PAYLOAD-TRANSPARENT through tag layers: the walk unwraps uninterpreted tags, so a
//! path over a tagged root resolves, and the subtree copier preserves tags.

use jqf_codec_core::{
    AccessInput, AccessOutcome, AccessResult, AccessSession, CodecError, CodecFailureKind, CodecRunContext,
    DocumentProduct, ExactSelectionRecord, LocatedOutcome, OwnedStep, PortableStep, SelectionOrigin, own_steps,
};
use jqf_data::AccountedSemanticNode;
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use alloc::vec::Vec;

use crate::parse::{SCALAR_KIND, data_contract, decode_span, fresh_builder, map_data};
use crate::walk::{self, Located};

/// The one input shape these routes serve: a raw source range (the record drive hands each route a byte range). A
/// document input is the binder's floor, never a native route — the same prologue every session's decode opens with.
fn source_input<'s>(input: &AccessInput<'_, 's>) -> Result<ResolvedSource<'s>, CodecError> {
    match input {
        AccessInput::Source(source) => Ok(*source),
        AccessInput::Document(_) => Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch)),
    }
}

/// Builds the null-product for a negative located observation (missing member or kind mismatch), carrying the exact
/// selection record.
fn negative_outcome(
    located: &Located,
    origin: SelectionOrigin,
    resources: &mut ResourceContext<'_>,
) -> Result<(DocumentProduct<'static>, ExactSelectionRecord), CodecError> {
    let (mut builder, _schema) = fresh_builder(resources)?;
    let root = builder
        .add_node(SCALAR_KIND, AccountedSemanticNode::Null, None, resources)
        .map_err(map_data)?;
    let selection = match located {
        Located::Missing { step } => ExactSelectionRecord::Missing {
            step_index: *step,
            origin,
        },
        Located::TypeMismatch { step, actual } => ExactSelectionRecord::TypeMismatch {
            step_index: *step,
            actual_type: *actual,
            origin,
            hint: None,
        },
        Located::Value { .. } => return Err(data_contract()),
    };
    let document = builder.finish(root, resources).map_err(map_data)?;
    let product = DocumentProduct::try_new(document, resources)?;
    Ok((product, selection))
}

// ---------------------------------------------------------------------------

/// Native located session: walk, decode the located span, publish a [`LocatedOutcome`] (negative observations share the
/// null-product arm verbatim).
pub(crate) struct NativeLocatedSession {
    steps: Vec<OwnedStep>,
    origin: SelectionOrigin,
    /// The adjacent-value opt-in: the walk stops at the first top-level item and the session reports the item's end as
    /// the consumed offset, so the drive advances by exactly one item.
    adjacent: bool,
    finished: bool,
}

impl NativeLocatedSession {
    pub(crate) fn try_new(
        steps: &[PortableStep],
        origin: SelectionOrigin,
        allow_adjacent_values: bool,
    ) -> Result<Self, CodecError> {
        Ok(Self {
            steps: own_steps(steps)?,
            origin,
            adjacent: allow_adjacent_values,
            finished: false,
        })
    }

    /// Reuses this session for one more adjacent value. Keeps the owned steps when the path is unchanged so a stream of
    /// the same extraction does not re-copy the member identities.
    pub(crate) fn try_reset(
        &mut self,
        steps: &[PortableStep],
        origin: SelectionOrigin,
        allow_adjacent_values: bool,
    ) -> Result<bool, CodecError> {
        if !steps_match(&self.steps, steps) {
            self.steps = own_steps(steps)?;
        }
        self.origin = origin;
        self.adjacent = allow_adjacent_values;
        self.finished = false;
        Ok(true)
    }

    fn decode_located<'source>(
        &mut self,
        source: ResolvedSource<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        if self.finished {
            return Err(data_contract());
        }
        self.finished = true;
        let (located, item_end) = walk::locate(source, self.steps.as_slice(), self.adjacent, resources)?;
        let (product, selection) = match located {
            Located::Value { start, end, .. } => {
                let product = decode_span(source, start, end, resources)?;
                let selection = ExactSelectionRecord::Node {
                    node: product
                        .document()
                        .node_handle(product.document().root())
                        .map_err(map_data)?,
                    origin: self.origin,
                };
                (product, selection)
            }
            negative @ (Located::Missing { .. } | Located::TypeMismatch { .. }) => {
                negative_outcome(&negative, self.origin, resources)?
            }
        };
        let outcome = LocatedOutcome::try_new(&product, selection)?;
        // The consumed-offset receipt: under the adjacent-value opt-in the session publishes the offset of the END of
        // the first top-level item, so the drive advances by exactly one item and decodes the remainder as the next
        // one.
        if self.adjacent {
            let consumed = u64::try_from(item_end).unwrap_or(u64::MAX);
            Ok(AccessResult::from_outcome_with_consumed_offset(
                AccessOutcome::Located(outcome),
                consumed,
            ))
        } else {
            Ok(AccessResult::from_outcome(AccessOutcome::Located(outcome)))
        }
    }
}

fn steps_match(owned: &[OwnedStep], portable: &[PortableStep]) -> bool {
    if owned.len() != portable.len() {
        return false;
    }
    owned.iter().zip(portable).all(|(left, right)| match (left, right) {
        (OwnedStep::Member(member), PortableStep::SemanticMember(other)) => member == other,
        (OwnedStep::Index(index), PortableStep::SemanticIndex(other)) => index == other,
        (
            OwnedStep::Range { start, end },
            PortableStep::SemanticRange {
                start: other_start,
                end: other_end,
            },
        ) => start == other_start && end == other_end,
        _ => false,
    })
}

impl AccessSession for NativeLocatedSession {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
    fn decode<'source>(
        &mut self,
        input: AccessInput<'_, 'source>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        let source = source_input(&input)?;
        self.decode_located(source, context.resources())
    }
}
