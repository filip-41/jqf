//! Native located access route over the validate-only walk.

use jqf_codec_core::{
    AccessInput, AccessOutcome, AccessResult, AccessSession, CodecError, CodecFailureKind, CodecRunContext,
    DocumentProduct, ExactSelectionRecord, LocatedOutcome, OwnedStep, PortableStep, SelectionOrigin, own_steps,
};
use jqf_data::{AccountedSemanticNode, NodeId};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use alloc::vec::Vec;

use crate::materialize;
use crate::options::Dialect;
use crate::scan;
use crate::walk::{self, Located};

pub(crate) struct NativeLocatedSession {
    steps: Vec<OwnedStep>,
    origin: SelectionOrigin,
    dialect: Dialect,
    /// Re-anchored kept-subtree prune over the located span. `None` keeps every member.
    prune: Option<materialize::PruneLookup>,
    /// Kind-only span: empty container or dummy scalar from the first payload byte.
    type_demand: bool,
    finished: bool,
}

impl NativeLocatedSession {
    pub(crate) fn try_new(
        steps: &[PortableStep],
        origin: SelectionOrigin,
        dialect: Dialect,
        prune: Option<materialize::PruneLookup>,
        type_demand: bool,
    ) -> Result<Self, CodecError> {
        Ok(Self {
            steps: own_steps(steps)?,
            origin,
            dialect,
            prune,
            type_demand,
            finished: false,
        })
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
        let (located, _item_end) = walk::locate(source, self.dialect, self.steps.as_slice(), resources)?;
        let (product, selection) = match located {
            Located::Value { start, end, .. } => {
                let product = if self.type_demand {
                    kind_only_span(source, start, self.dialect, resources)?
                } else {
                    decode_span(source, start, end, self.dialect, self.prune.as_ref(), resources)?
                };
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
                negative_outcome(&negative, self.origin, self.dialect.id(), resources)?
            }
        };
        let outcome = LocatedOutcome::try_new(&product, selection)?;
        Ok(AccessResult::from_outcome(AccessOutcome::Located(outcome)))
    }
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
        let source = match input {
            AccessInput::Source(source) => source,
            AccessInput::Document(_) => {
                return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
            }
        };
        self.decode_located(source, context.resources())
    }
}

fn kind_only_span(
    source: ResolvedSource<'_>,
    start: usize,
    dialect: Dialect,
    resources: &mut ResourceContext<'_>,
) -> Result<DocumentProduct<'static>, CodecError> {
    let kind = walk::classify_kind(source, dialect, start, resources)?;
    let (builder, root) = materialize::kind_only_document(dialect, kind, resources)?;
    let document = builder.finish(root, resources).map_err(map_data)?;
    DocumentProduct::try_new(document, resources)
}

fn decode_span(
    source: ResolvedSource<'_>,
    start: usize,
    end: usize,
    dialect: Dialect,
    prune: Option<&materialize::PruneLookup>,
    resources: &mut ResourceContext<'_>,
) -> Result<DocumentProduct<'static>, CodecError> {
    let span_source = ResolvedSource::new(
        source.source(),
        source.label(),
        &source.bytes()[start..end],
        source
            .base_offset()
            .saturating_add(u64::try_from(start).unwrap_or(u64::MAX)),
    );
    let mut run = CodecRunContext::new(resources);
    run.set_cooperative_credits(4_096);
    let skeleton = scan::scan(span_source, dialect, &mut run)?;
    let (builder, root) =
        materialize::build_document_with_spans(&skeleton, dialect, span_source, false, None, prune, false, resources)?;
    let document = builder.finish(root, resources).map_err(map_data)?;
    DocumentProduct::try_new(document, resources)
}

fn negative_outcome(
    located: &Located,
    origin: SelectionOrigin,
    dialect_id: &str,
    resources: &mut ResourceContext<'_>,
) -> Result<(DocumentProduct<'static>, ExactSelectionRecord), CodecError> {
    // The recipe names the SESSION's dialect, not a fixed one: a negative outcome under the wire or key-equivalence
    // dialect must record the identity that actually served it. The kind and role identities are materialize's own
    // constants — one spelling of the schema inventory.
    let recipe = jqf_data::DocumentSchemaRecipe::try_new(
        crate::FORMAT_ID,
        Some(dialect_id),
        materialize::NODE_KINDS,
        materialize::OCCURRENCE_ROLES,
        &[],
        &[],
    )
    .map_err(map_data)?;
    let (mut builder, _schema) = jqf_data::AccountedDocumentBuilder::try_new_prepared_with_coverage(
        &recipe,
        jqf_data::BuilderCoverage::minimal_semantic(),
    )
    .map_err(map_data)?;
    let root: NodeId = builder
        .add_node("messagepack.scalar@1", AccountedSemanticNode::Null, None, resources)
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

fn map_data(error: jqf_data::DataError) -> CodecError {
    jqf_codec_core::map_data(error, "MessagePack builder rejected document construction")
}

fn data_contract() -> CodecError {
    jqf_codec_core::data_contract("MessagePack native located session")
}
