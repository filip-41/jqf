//! Located TOML access: whole-input validate, then materialize only the hit.
//!
//! An empty path uses [`crate::parse::parse_direct`] and [`crate::locate`]. A non-empty path uses
//! [`crate::walk::Walker`]. The published outcome matches whole-document-then-navigate.
//!
//! Count/element Exact publishes the walk's child cardinality: a contiguous value uses
//! [`crate::parse::publish_located_skeleton`]; a `[table]`, implicit dotted table, or range uses
//! [`crate::parse::publish_walk_skeleton`]. Print of a non-contiguous hit still concatenates and
//! re-parses through [`crate::parse::parse_direct`] — there is no contiguous source region.
//!
//! The published [`AccessOutcome::Located`] carries the identical [`ExactSelectionRecord`] the
//! whole-decode-then-navigate path publishes, so the SDK/CLI behaviour (located value / `null` / typed error) is
//! byte-identical. The negative observations publish a null product, exactly the floor's own `null` for a missing or
//! mismatched path.

use alloc::vec::Vec;
use jqf_codec_core::{
    AccessInput, AccessOutcome, AccessResult, AccessSession, CodecError, CodecFailureKind, CodecRunContext,
    DocumentProduct, ExactSelectionRecord, LocatedOutcome, PortableStep, SelectionOrigin,
};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::locate::{self, OwnedStep};
use crate::materialize;
use crate::parse::{self, data_contract, map_data};
use crate::provider::DialectKind;

/// Native scoped session state stored in the core-owned tracked carrier.
pub(crate) struct NativeScopedSession {
    steps: Vec<OwnedStep>,
    origin: SelectionOrigin,
    dialect: DialectKind,
    coverage: jqf_data::BuilderCoverage,
    /// Count/element Exact: publish a contiguous value container as a cached span.
    skeleton: bool,
    /// Whether the one-shot locate+materialize poll already ran.
    finished: bool,
}

impl NativeScopedSession {
    pub(crate) fn try_new(
        steps: &[PortableStep],
        origin: SelectionOrigin,
        dialect: DialectKind,
        coverage: jqf_data::BuilderCoverage,
        skeleton: bool,
    ) -> Result<Self, CodecError> {
        Ok(Self {
            steps: locate::own_steps(steps)?,
            origin,
            dialect,
            coverage,
            skeleton,
            finished: false,
        })
    }

    fn poll_scoped<'source>(
        &mut self,
        source: ResolvedSource<'source>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        if source.bytes().len() > u32::MAX as usize {
            return Err(CodecError::new(CodecFailureKind::Overflow));
        }
        if self.finished {
            return Err(data_contract());
        }
        self.finished = true;
        if self.steps.is_empty() {
            // The root selection IS the whole document: the tree path, whose parse cost is the answer's own cost.
            let resources = context.resources();
            let mut doc = parse::parse_direct(source, self.dialect, resources)?;
            let root = doc.subtree(&crate::grammar::Path::default());
            let located = locate::locate(&root, self.steps.as_slice())?;
            let (builder, root) =
                materialize::build_located_document(&located, doc.names(), source.bytes(), self.coverage, resources)?;
            let document = builder.finish(root, resources).map_err(map_data)?;
            let product = DocumentProduct::try_new(document, resources)?;
            return self.publish(&product, &located);
        }
        // The byte-level validate + navigate walk: validates the whole input to the parser's exact strictness and
        // resolves the target path without building the tree.
        let walker = crate::walk::Walker::try_new(
            source,
            self.dialect,
            self.steps.as_slice(),
            context.resources(),
            self.coverage.attached_facts(),
        );
        let located_walk = walker.walk()?;
        if self.skeleton
            && let Some(product) = self.try_publish_skeleton(source, &located_walk, context)?
        {
            return self.publish_walk(&product, &located_walk);
        }
        let (builder, root) = self.materialize_walk(source, &located_walk, context.resources())?;
        let document = builder.finish(root, context.resources()).map_err(map_data)?;
        let product = DocumentProduct::try_new(document, context.resources())?;
        self.publish_walk(&product, &located_walk)
    }

    /// Count/element Exact: publish the walk's child cardinality. Contiguous arrays and inline tables keep the
    /// Value span arm; `[table]`, implicit dotted tables, and ranges have no rematerializable value region and
    /// publish a kind-witness skeleton from walk facts. Print still re-parses non-contiguous hits
    /// through [`crate::parse::parse_direct`].
    fn try_publish_skeleton<'source>(
        &self,
        source: ResolvedSource<'source>,
        located: &crate::walk::LocatedWalk,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<Option<DocumentProduct<'source>>, CodecError> {
        match located {
            crate::walk::LocatedWalk::Value {
                start,
                end,
                child_count,
                container: Some(kind),
                ..
            } => parse::publish_located_skeleton(source, *start, *end, *kind, *child_count, self.dialect, context)
                .map(Some),
            crate::walk::LocatedWalk::Table {
                child_count, container, ..
            } => parse::publish_walk_skeleton(*container, *child_count, self.dialect, context).map(Some),
            crate::walk::LocatedWalk::ImplicitTable { child_count, .. } => {
                parse::publish_walk_skeleton(jqf_data::ContainerSpanKind::Object, *child_count, self.dialect, context)
                    .map(Some)
            }
            crate::walk::LocatedWalk::RangeValue { child_count, .. } => {
                parse::publish_walk_skeleton(jqf_data::ContainerSpanKind::Array, *child_count, self.dialect, context)
                    .map(Some)
            }
            crate::walk::LocatedWalk::RangeTables { elements } => parse::publish_walk_skeleton(
                jqf_data::ContainerSpanKind::Array,
                u64::try_from(elements.len()).unwrap_or(u64::MAX),
                self.dialect,
                context,
            )
            .map(Some),
            crate::walk::LocatedWalk::Value { container: None, .. }
            | crate::walk::LocatedWalk::Missing { .. }
            | crate::walk::LocatedWalk::TypeMismatch { .. } => Ok(None),
        }
    }

    /// Turns the walk's located answer into a fresh document: a contiguous value region re-parsed by wrapping, a
    /// table's collected statement spans re-parsed by concatenation, a range's region or element spans, or the null
    /// product of a negative observation.
    fn materialize_walk(
        &self,
        source: jqf_source::ResolvedSource<'_>,
        located: &crate::walk::LocatedWalk,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(jqf_data::AccountedDocumentBuilder<'static>, jqf_data::NodeId), CodecError> {
        match located {
            crate::walk::LocatedWalk::Value {
                start,
                end,
                leading,
                inline,
                ..
            } => crate::lazy::build_wrapped_value(
                source.bytes(),
                *start,
                *end,
                leading,
                inline,
                self.dialect,
                self.coverage,
                resources,
            ),
            crate::walk::LocatedWalk::Table {
                spans,
                foot,
                key_depth,
                element,
                ..
            } => crate::lazy::build_statement_table(
                source.bytes(),
                spans,
                foot,
                *key_depth,
                *element,
                self.dialect,
                self.coverage,
                resources,
            ),
            crate::walk::LocatedWalk::ImplicitTable { pieces, .. } => {
                crate::lazy::build_implicit_table(source.bytes(), pieces, self.dialect, self.coverage, resources)
            }
            crate::walk::LocatedWalk::RangeValue { start, end, empty, .. } => crate::lazy::build_range_value(
                source.bytes(),
                *start,
                *end,
                *empty,
                self.dialect,
                self.coverage,
                resources,
            ),
            crate::walk::LocatedWalk::RangeTables { elements } => {
                crate::lazy::build_range_of_tables(source.bytes(), elements, self.dialect, self.coverage, resources)
            }
            crate::walk::LocatedWalk::Missing { .. } | crate::walk::LocatedWalk::TypeMismatch { .. } => {
                // The negative observations publish the null product, exactly as the tree path does.
                materialize::build_located_document(
                    &locate::Located::Missing { step: 0 },
                    &[],
                    source.bytes(),
                    self.coverage,
                    resources,
                )
            }
        }
    }

    /// Publishes the located product under the scoped selection record, so a missing path and a mismatch read
    /// identically on the whole route.
    fn publish_walk<'source>(
        &self,
        product: &DocumentProduct<'source>,
        located: &crate::walk::LocatedWalk,
    ) -> Result<AccessResult<'source>, CodecError> {
        let selection = match located {
            // A materialized range is an ordinary NODE selection: the published value is the fresh array the slice
            // produced.
            crate::walk::LocatedWalk::Value { .. }
            | crate::walk::LocatedWalk::Table { .. }
            | crate::walk::LocatedWalk::ImplicitTable { .. }
            | crate::walk::LocatedWalk::RangeValue { .. }
            | crate::walk::LocatedWalk::RangeTables { .. } => ExactSelectionRecord::Node {
                node: product.document().root_handle(),
                origin: self.origin,
            },
            crate::walk::LocatedWalk::Missing { step } => ExactSelectionRecord::Missing {
                step_index: *step,
                origin: self.origin,
            },
            crate::walk::LocatedWalk::TypeMismatch { step, actual } => ExactSelectionRecord::TypeMismatch {
                step_index: *step,
                actual_type: *actual,
                origin: self.origin,
                hint: None,
            },
        };
        let outcome = LocatedOutcome::try_new(product, selection)?;
        Ok(AccessResult::from_outcome(AccessOutcome::Located(outcome)))
    }

    /// Publishes the located product under the scoped selection record, so a missing path and a mismatch read
    /// identically on the whole route.
    fn publish<'source>(
        &self,
        product: &DocumentProduct<'source>,
        located: &locate::Located<'_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        let selection = match located {
            // A materialized range is an ordinary NODE selection: the published value is the fresh array the slice
            // produced.
            locate::Located::Value(_) | locate::Located::Table(_) | locate::Located::ArrayOfTables(_) => {
                ExactSelectionRecord::Node {
                    node: product.document().root_handle(),
                    origin: self.origin,
                }
            }
            locate::Located::Missing { step } => ExactSelectionRecord::Missing {
                step_index: *step,
                origin: self.origin,
            },
        };
        let outcome = LocatedOutcome::try_new(product, selection)?;
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
