//! Native Exact/Located session for jqft and jqfjson text.
//!
//! Direct Exact republishes the selection as the product root
//! (`ExactSelectionRecord::Node { node: product.document().root_handle(), origin }`). The walk is a locate mode of
//! the ordinary iterative parser: every byte is validated, last-value-wins still scans later keys, and count/element
//! Exact publishes the locate span with the cached child count. Print without that hint re-parses only the located
//! span. A range step declines [`CodecFailureKind::RequirementMismatch`] so the binder's whole-document
//! floor serves it. Attribute demand does not Direct-bind this slot.

use jqf_codec_core::{
    AccessInput, AccessOutcome, AccessResult, AccessSession, CodecError, CodecFailureKind, CodecRunContext,
    DocumentProduct, ExactSelectionRecord, LocatedOutcome, OwnedStep, PortableStep, SelectionOrigin, own_steps,
};
use jqf_data::{
    AccountedDocumentBuilder, AccountedSemanticNode, AuthoritativeEmptyFamilies, BuilderCoverage, ContainerSpanKind,
    DiagnosticCoverage, DocumentCapabilityFamily, DocumentCapacity, DocumentSourceBindingPoll,
    DocumentSourceBindingStage, LazySpanMaterializer,
};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::locate::Located;
use crate::parse::{self, JqftParseState};
use crate::provider::{self, JqftKind};

/// Native scoped session stored in the core-owned tracked carrier.
pub(crate) struct NativeScopedSession {
    steps: alloc::vec::Vec<OwnedStep>,
    origin: SelectionOrigin,
    kind: JqftKind,
    coverage: BuilderCoverage,
    allow_adjacent_values: bool,
    /// Count/element Exact: publish the locate span instead of re-parsing the hit.
    skeleton: bool,
    finished: bool,
}

impl NativeScopedSession {
    pub(crate) fn try_new(
        steps: &[PortableStep],
        origin: SelectionOrigin,
        kind: JqftKind,
        coverage: BuilderCoverage,
        allow_adjacent_values: bool,
        skeleton: bool,
    ) -> Result<Self, CodecError> {
        Ok(Self {
            steps: own_steps(steps)?,
            origin,
            kind,
            coverage,
            allow_adjacent_values,
            skeleton,
            finished: false,
        })
    }

    fn poll_scoped<'source>(
        &mut self,
        source: ResolvedSource<'source>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        if self.finished {
            return Err(parse::data_contract());
        }
        if self.steps.iter().any(|step| matches!(step, OwnedStep::Range { .. })) {
            // Decline after the ordinary validate walk so a corrupt unread byte
            // still fails Exact as Whole; the SDK then rebinds Whole for the slice.
            let mut locate = JqftParseState::try_new_locate(
                source,
                self.kind,
                self.allow_adjacent_values,
                self.coverage,
                alloc::vec::Vec::new(),
            );
            locate.poll_locate(source, context)?;
            self.finished = true;
            return Err(decline_located_range());
        }
        self.finished = true;
        if self.steps.is_empty() {
            return self.decode_identity(source, context);
        }
        let mut locate = JqftParseState::try_new_locate(
            source,
            self.kind,
            self.allow_adjacent_values,
            self.coverage,
            core::mem::take(&mut self.steps),
        );
        let observation = locate.poll_locate(source, context)?;
        let document_end = locate.document_end();
        match observation {
            Located::Node { container: Some(_), .. } if self.skeleton => {
                self.publish_located_skeleton(source, observation, document_end, context)
            }
            Located::Node { start, end, .. } => self.materialize_span(source, start, end, document_end, context),
            negative @ (Located::Missing { .. } | Located::TypeMismatch { .. }) => {
                self.publish_negative(source, negative, document_end, context.resources())
            }
        }
    }

    /// Empty path: the selection is the document root. One Whole parse, published as Located with the root handle.
    fn decode_identity<'source>(
        &self,
        source: ResolvedSource<'source>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        let mut parse = JqftParseState::try_new(source, self.kind, self.allow_adjacent_values, self.coverage);
        let (outcome, report) = parse.decode(AccessInput::Source(source), context)?.into_parts();
        let AccessOutcome::FullDocument(product) = outcome else {
            return Err(parse::data_contract());
        };
        self.publish_node(&product, report.consumed_offset())
    }

    fn materialize_span<'source>(
        &self,
        source: ResolvedSource<'source>,
        start: usize,
        end: usize,
        document_end: usize,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        let bytes = source.bytes().get(start..end).ok_or_else(parse::data_contract)?;
        let base = source
            .base_offset()
            .saturating_add(u64::try_from(start).unwrap_or(u64::MAX));
        let sub = ResolvedSource::new(source.source(), source.label(), bytes, base);
        let mut materialize = JqftParseState::try_new_value(self.kind, self.coverage);
        let (outcome, _) = materialize.decode(AccessInput::Source(sub), context)?.into_parts();
        let AccessOutcome::FullDocument(product) = outcome else {
            return Err(parse::data_contract());
        };
        let consumed = self
            .allow_adjacent_values
            .then_some(u64::try_from(document_end).unwrap_or(u64::MAX));
        self.publish_node(&product, consumed)
    }

    /// Publishes the last-wins located container as a lazy span root. The locate
    /// walk already proved `[start, end)` and counted its children.
    #[allow(
        unsafe_code,
        reason = "span admission and source attach are unsafe by jqf-data; the locate walk proved this range"
    )]
    fn publish_located_skeleton<'source>(
        &self,
        source: ResolvedSource<'source>,
        located: Located,
        document_end: usize,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        let Located::Node {
            start,
            end,
            child_count,
            container: Some(container),
        } = located
        else {
            return Err(parse::data_contract());
        };
        let bytes = source.bytes().get(start..end).ok_or_else(parse::data_contract)?;
        let base = source
            .base_offset()
            .saturating_add(u64::try_from(start).unwrap_or(u64::MAX));
        let sub = ResolvedSource::new(source.source(), source.label(), bytes, base);
        let recipe = provider::jqft_recipe(self.kind).map_err(parse::map_data)?;
        let (mut builder, schema) = AccountedDocumentBuilder::try_new_prepared_with_coverage(&recipe, self.coverage)
            .map_err(parse::map_data)?;
        builder.set_authoritative_empty_families(AuthoritativeEmptyFamilies::from_family(
            DocumentCapabilityFamily::Attributes,
        ));
        builder.set_diagnostic_coverage(DiagnosticCoverage::NotRequested);
        builder.bind_span_materializer(span_materializer(self.kind));
        let mut stage = DocumentSourceBindingStage::new(sub).map_err(parse::map_data)?;
        let binding = loop {
            // SAFETY: codec-core holds this session's source unchanged; `sub` is
            // the last-wins range the locate walk recorded on that authority.
            match unsafe { stage.poll(sub, context.resources()) }.map_err(parse::map_data)? {
                DocumentSourceBindingPoll::Pending => context.replenish_work()?,
                DocumentSourceBindingPoll::Ready(binding) => break binding,
            }
        };
        builder.bind_source(binding).map_err(parse::map_data)?;
        let span = jqf_source::Span::from_usize(0, bytes.len());
        let slot = match container {
            ContainerSpanKind::Array => 10,
            ContainerSpanKind::Object => 11,
        };
        let kind = schema.node_kind(slot).ok_or_else(parse::data_contract)?;
        // SAFETY: the locate walk proved `[start, end)` one complete container.
        let root = unsafe {
            builder.add_prepared_bound_container_span_node(&schema, kind, span, container, context.resources())
        }
        .map_err(parse::map_data)?;
        builder
            .set_container_span_counts(root, child_count, None)
            .map_err(parse::map_data)?;
        let document = builder.finish(root, context.resources()).map_err(parse::map_data)?;
        let document = unsafe { document.with_borrowed_source_from_bound_authority(sub, context.resources()) }
            .map_err(parse::map_data)?;
        let product = DocumentProduct::try_new(document, context.resources())?;
        let consumed = self
            .allow_adjacent_values
            .then_some(u64::try_from(document_end).unwrap_or(u64::MAX));
        self.publish_node(&product, consumed)
    }

    fn publish_node<'source>(
        &self,
        product: &DocumentProduct<'source>,
        consumed: Option<u64>,
    ) -> Result<AccessResult<'source>, CodecError> {
        let selection = ExactSelectionRecord::Node {
            node: product.document().root_handle(),
            origin: self.origin,
        };
        let outcome = LocatedOutcome::try_new(product, selection)?;
        Ok(publish(AccessOutcome::Located(outcome), consumed))
    }

    fn publish_negative<'source>(
        &self,
        source: ResolvedSource<'source>,
        located: Located,
        document_end: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        let product = null_product(self.kind, source, resources)?;
        let selection = match located {
            Located::Missing { step } => ExactSelectionRecord::Missing {
                step_index: step,
                origin: self.origin,
            },
            Located::TypeMismatch { step, actual } => ExactSelectionRecord::TypeMismatch {
                step_index: step,
                actual_type: actual,
                origin: self.origin,
                hint: None,
            },
            Located::Node { .. } => return Err(parse::data_contract()),
        };
        let outcome = LocatedOutcome::try_new(&product, selection)?;
        let consumed = self
            .allow_adjacent_values
            .then_some(u64::try_from(document_end).unwrap_or(u64::MAX));
        Ok(publish(AccessOutcome::Located(outcome), consumed))
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

fn publish(outcome: AccessOutcome<'_>, consumed: Option<u64>) -> AccessResult<'_> {
    match consumed {
        Some(offset) => AccessResult::from_outcome_with_consumed_offset(outcome, offset),
        None => AccessResult::from_outcome(outcome),
    }
}

fn null_product(
    kind: JqftKind,
    source: ResolvedSource<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<DocumentProduct<'static>, CodecError> {
    let _ = source;
    let recipe = provider::jqft_recipe(kind).map_err(parse::map_data)?;
    let (mut builder, _schema) =
        AccountedDocumentBuilder::try_new_prepared_with_coverage(&recipe, BuilderCoverage::minimal_semantic())
            .map_err(parse::map_data)?;
    let _ = builder.try_reserve(
        DocumentCapacity {
            nodes: 1,
            ..DocumentCapacity::default()
        },
        resources,
    );
    builder.set_authoritative_empty_families(AuthoritativeEmptyFamilies::from_family(
        DocumentCapabilityFamily::Attributes,
    ));
    builder.set_diagnostic_coverage(DiagnosticCoverage::NotRequested);
    let prefix = kind.schema_prefix();
    let root = builder
        .add_node(
            provider::kind_for(prefix, &AccountedSemanticNode::Null),
            AccountedSemanticNode::Null,
            None,
            resources,
        )
        .map_err(parse::map_data)?;
    let document = builder.finish(root, resources).map_err(parse::map_data)?;
    DocumentProduct::try_new(document, resources)
}

struct JqftSpanMaterializer {
    kind: JqftKind,
}

impl LazySpanMaterializer for JqftSpanMaterializer {
    fn materialize_span(
        &self,
        text: &str,
        resources: &mut ResourceContext<'_>,
    ) -> Result<jqf_data::Value, jqf_data::DataError> {
        materialize_jqft_span(self.kind, text.as_bytes(), resources)
    }

    fn materialize_span_bytes(
        &self,
        bytes: &[u8],
        resources: &mut ResourceContext<'_>,
    ) -> Result<jqf_data::Value, jqf_data::DataError> {
        materialize_jqft_span(self.kind, bytes, resources)
    }
}

fn materialize_jqft_span(
    kind: JqftKind,
    bytes: &[u8],
    resources: &mut ResourceContext<'_>,
) -> Result<jqf_data::Value, jqf_data::DataError> {
    use jqf_source::{SourceId, SourceKind, SourceRef};
    const SPAN_SOURCE: SourceRef = SourceRef::new(SourceId::new(0), SourceKind::Input);
    let source = ResolvedSource::new(SPAN_SOURCE, "container-span", bytes, 0);
    let mut parse = JqftParseState::try_new_value(kind, BuilderCoverage::minimal_semantic());
    let mut run = CodecRunContext::new(resources);
    run.set_cooperative_credits(4_096);
    let result = parse
        .decode(AccessInput::Source(source), &mut run)
        .map_err(|error| jqf_codec_core::map_span_materialization_error(&error))?;
    let AccessOutcome::FullDocument(product) = result.into_parts().0 else {
        return Err(jqf_data::DataError::InvalidDocument);
    };
    product.document().materialize_root(resources)
}

static JQFT_SPAN_MATERIALIZER: JqftSpanMaterializer = JqftSpanMaterializer { kind: JqftKind::Jqft };
static JQFJSON_SPAN_MATERIALIZER: JqftSpanMaterializer = JqftSpanMaterializer {
    kind: JqftKind::Jqfjson,
};

fn span_materializer(kind: JqftKind) -> &'static dyn LazySpanMaterializer {
    match kind {
        JqftKind::Jqft => &JQFT_SPAN_MATERIALIZER,
        JqftKind::Jqfjson => &JQFJSON_SPAN_MATERIALIZER,
    }
}

/// A located route that cannot serve the demanded step shape declines, so the binder's whole-document floor serves the
/// demand instead.
fn decline_located_range() -> CodecError {
    CodecError::new(CodecFailureKind::RequirementMismatch)
}
