//! Generic exact interpreter over complete format-neutral document authority.
//!
//! [`ExactFallbackState`] is the one interpreter here: it resolves one exact path to a single located observation over
//! an already materialized [`DocumentProduct`], through the [`AccessAdapter`] kind selected at open. Whole-document
//! demands are NOT served here: the dedicated floor adapters elsewhere (the whole-document demand fallback and its
//! attribute-absence twins) own that shape.

use jqf_resource::WorkAdmission;

use crate::capability::AccessResultKind;
use crate::pattern::{ExactPath, PortableStep};
use crate::schedule::SelectionOrigin;
use crate::{
    AccessAdapter, AccessInput, AccessOutcome, AccessReport, AccessResult, AccessSession, CodecError, CodecFailureKind,
    CodecRunContext, DocumentProduct, ExactSelectionRecord, LocatedOutcome,
};

pub(crate) struct ExactFallbackState {
    result: AccessResultKind,
    diagnostics: jqf_data::DiagnosticCoverage,
    adapter: AccessAdapter,
    origin: Option<SelectionOrigin>,
    path: Option<ExactPath>,
}

impl ExactFallbackState {
    pub(crate) const fn new(
        result: AccessResultKind,
        diagnostics: jqf_data::DiagnosticCoverage,
        adapter: AccessAdapter,
        origin: Option<SelectionOrigin>,
        path: Option<ExactPath>,
    ) -> Self {
        Self {
            result,
            diagnostics,
            adapter,
            origin,
            path,
        }
    }
}

impl AccessSession for ExactFallbackState {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
    fn decode<'source>(
        &mut self,
        input: AccessInput<'_, 'source>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        let AccessInput::Document(product) = input else {
            return Err(contract());
        };
        // Production seals this interpreter only for Located requirements (the floor adapters own the whole-document
        // shape), so the CompleteDocument arm this state once served is gone. The assert pins that assumption instead
        // of silently ignoring a mis-sealed plan.
        debug_assert_eq!(
            self.result,
            AccessResultKind::Located,
            "whole-document demands are served by the dedicated floor adapters, not here"
        );
        let path = self.path.as_ref().ok_or_else(contract)?;
        let origin = self.origin.ok_or_else(contract)?;
        // The walk is LOCAL: one decode resolves the whole path or fails, and a failed decode is terminal, so there is
        // no cursor to persist and no partial position for a second call to resume from.
        let missing = |cursor| {
            result(
                product,
                ExactSelectionRecord::Missing {
                    step_index: cursor,
                    origin,
                },
                self.diagnostics,
                self.adapter,
            )
        };
        let mismatch = |cursor,
                        kind: Result<jqf_data::ValueKind, jqf_data::DataError>|
         -> Result<AccessResult<'source>, CodecError> {
            result(
                product,
                ExactSelectionRecord::TypeMismatch {
                    step_index: cursor,
                    actual_type: kind.map_err(|_| contract())?,
                    origin,
                    hint: None,
                },
                self.diagnostics,
                self.adapter,
            )
        };
        let mut node = product.document().root_handle();
        let mut cursor = 0;
        while let Some(step) = path.steps().get(cursor) {
            if context.resources().admit_work_transition()? == WorkAdmission::Pending {
                // Straight-line decode: replenish the cooperative budget and continue the walk instead of yielding a
                // poll.
                context.replenish_work()?;
                continue;
            }
            // The payload-transparent view: tag LAYER nodes (CBOR's uninterpreted tags) are descended to their single
            // payload, so the walk's kind and projections see through the wrapper exactly as the engine's
            // `located_view` law reads a located node. The per-codec located walks this interpreter replaces all
            // navigate payload-transparently (the memo's kind law), so a mismatch reports the PAYLOAD kind, never the
            // wrapper's.
            let view = product.document().payload_view(node).map_err(|_| contract())?;
            node = match step {
                PortableStep::SemanticMember(key) => match view.object().map_err(|_| contract())? {
                    Some(object) => match object.get(key.as_str()) {
                        Some(value) => product.document().node_handle(value.node()).map_err(|_| contract())?,
                        None => return missing(cursor),
                    },
                    None => {
                        return mismatch(cursor, view.kind());
                    }
                },
                PortableStep::SemanticIndex(index) => match view.array().map_err(|_| contract())? {
                    Some(array) => {
                        let position = if *index < 0 {
                            i64::try_from(array.len())
                                .ok()
                                .and_then(|len| len.checked_add(*index))
                                .and_then(|value| usize::try_from(value).ok())
                        } else {
                            usize::try_from(*index).ok()
                        };
                        let Some(position) = position else {
                            return missing(cursor);
                        };
                        match array.get(position) {
                            Some(value) => product.document().node_handle(value.node()).map_err(|_| contract())?,
                            None => return missing(cursor),
                        }
                    }
                    None => {
                        return mismatch(cursor, view.kind());
                    }
                },
                // Unreachable: `plan_compatible` declines every adapter for a footprint carrying a range step
                // (NO-CORE-FALLBACK), so this interpreter never receives one. Failing loudly rather than approximating
                // is the point of the law.
                PortableStep::SemanticRange { .. } => return Err(contract()),
            };
            cursor += 1;
        }
        result(
            product,
            ExactSelectionRecord::Node { node, origin },
            self.diagnostics,
            self.adapter,
        )
    }
}

fn result<'source>(
    product: &DocumentProduct<'source>,
    record: ExactSelectionRecord,
    diagnostics: jqf_data::DiagnosticCoverage,
    adapter: AccessAdapter,
) -> Result<AccessResult<'source>, CodecError> {
    Ok(AccessResult::new(
        AccessOutcome::Located(LocatedOutcome::try_new(product, record)?),
        AccessReport::new(diagnostics, adapter),
    ))
}
fn contract() -> CodecError {
    CodecError::new(CodecFailureKind::InternalContractViolation {
        contract: "generic exact complete-document interpreter",
    })
}

#[cfg(test)]
mod tests {
    use super::ExactFallbackState;
    use crate::capability::AccessResultKind;
    use crate::pattern::ExactPath;
    use crate::schedule::SelectionOrigin;
    use crate::test_support::resources;
    use crate::{
        AccessAdapter, AccessInput, AccessOutcome, AccessSession, CodecRunContext, DocumentProduct,
        ExactSelectionRecord,
    };
    use jqf_data::{AccountedDocumentBuilder, AccountedSemanticNode, DiagnosticCoverage, LocalOwnerRef};
    use jqf_resource::ResourceContext;
    fn bool_product(resources: &ResourceContext<'_>) -> DocumentProduct<'static> {
        let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder");
        let root = builder
            .add_node("test.bool", AccountedSemanticNode::Bool(true), None, resources)
            .expect("root");
        DocumentProduct::try_new(builder.finish(root, resources).expect("document"), resources).expect("product")
    }

    fn array_product(resources: &ResourceContext<'_>) -> DocumentProduct<'static> {
        let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder");
        let first = builder
            .add_node("test.bool", AccountedSemanticNode::Bool(false), None, resources)
            .expect("first");
        let second = builder
            .add_node("test.bool", AccountedSemanticNode::Bool(true), None, resources)
            .expect("second");
        let root = builder
            .add_node(
                "test.array",
                AccountedSemanticNode::Array { item_role: "test.item" },
                None,
                resources,
            )
            .expect("root");
        builder
            .add_occurrence(LocalOwnerRef::Node(root), "test.item", None, first, resources)
            .expect("first edge");
        builder
            .add_occurrence(LocalOwnerRef::Node(root), "test.item", None, second, resources)
            .expect("second edge");
        DocumentProduct::try_new(builder.finish(root, resources).expect("document"), resources).expect("product")
    }

    #[test]
    fn exact_root_and_type_mismatch_retain_complete_authority() {
        let mut resources = resources();
        let product = bool_product(&resources);
        let origin = SelectionOrigin::new(4);
        let mut root = ExactFallbackState::new(
            AccessResultKind::Located,
            DiagnosticCoverage::NotRequested,
            AccessAdapter::CompleteDocumentExact,
            Some(origin),
            Some(ExactPath::try_new(&resources)),
        );
        let mut context = CodecRunContext::new(&mut resources);
        let result = root
            .decode(AccessInput::Document(&product), &mut context)
            .expect("decode");
        let AccessOutcome::Located(located) = result.outcome() else {
            panic!("located")
        };
        assert!(matches!(located.result(), ExactSelectionRecord::Node { origin: seen, .. } if *seen == origin));

        let mut path = ExactPath::try_new(context.resources());
        path.try_push_semantic_index(0, context.resources());
        let mut mismatch = ExactFallbackState::new(
            AccessResultKind::Located,
            DiagnosticCoverage::NotRequested,
            AccessAdapter::CompleteDocumentExact,
            Some(origin),
            Some(path),
        );
        let mut context = CodecRunContext::new(&mut resources);
        let result = mismatch
            .decode(AccessInput::Document(&product), &mut context)
            .expect("decode");
        let AccessOutcome::Located(located) = result.outcome() else {
            panic!("located")
        };
        assert!(
            matches!(located.result(), ExactSelectionRecord::TypeMismatch { step_index: 0, origin: seen, hint: None, .. } if *seen == origin)
        );
        assert_eq!(
            located.product().document().root_handle(),
            product.document().root_handle()
        );
    }

    #[test]
    fn signed_index_boundaries_and_missing_retain_authority() {
        let mut resources = resources();
        let product = array_product(&resources);
        let origin = SelectionOrigin::new(9);
        for (index, exists) in [
            (-1, true),
            (-2, true),
            (-3, false),
            (i64::MIN, false),
            (0, true),
            (1, true),
            (2, false),
            (i64::MAX, false),
        ] {
            let mut path = ExactPath::try_new(&resources);
            path.try_push_semantic_index(index, &resources);
            let mut state = ExactFallbackState::new(
                AccessResultKind::Located,
                DiagnosticCoverage::NotRequested,
                AccessAdapter::CompleteDocumentExact,
                Some(origin),
                Some(path),
            );
            let mut context = CodecRunContext::new(&mut resources);
            let result = state
                .decode(AccessInput::Document(&product), &mut context)
                .expect("decode");
            let AccessOutcome::Located(located) = result.outcome() else {
                panic!("located")
            };
            assert_eq!(
                matches!(located.result(), ExactSelectionRecord::Node { .. }),
                exists,
                "index {index}"
            );
            assert_eq!(
                located.product().document().root_handle(),
                product.document().root_handle()
            );
            if !exists {
                assert!(
                    matches!(located.result(), ExactSelectionRecord::Missing { step_index: 0, origin: seen } if *seen == origin)
                );
            }
        }
    }
}
