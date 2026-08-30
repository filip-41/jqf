//! Located TOML access: whole-input validate, then materialize only the hit.
//!
//! An empty path uses [`crate::parse::parse_direct`] and [`crate::locate`]. A non-empty path uses
//! [`crate::walk::Walker`]. The published outcome matches whole-document-then-navigate.
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

use crate::locate::{self, ScopedStep};
use crate::materialize;
use crate::parse::{self, data_contract, map_data};
use crate::provider::DialectKind;

/// Native scoped session state stored in the core-owned tracked carrier.
pub(crate) struct NativeScopedSession {
    steps: Vec<ScopedStep>,
    origin: SelectionOrigin,
    dialect: DialectKind,
    coverage: jqf_data::BuilderCoverage,
    /// Whether the one-shot locate+materialize poll already ran.
    finished: bool,
}

impl NativeScopedSession {
    pub(crate) fn try_new(
        steps: &[PortableStep],
        origin: SelectionOrigin,
        dialect: DialectKind,
        coverage: jqf_data::BuilderCoverage,
    ) -> Result<Self, CodecError> {
        Ok(Self {
            steps: locate::own_steps(steps)?,
            origin,
            dialect,
            coverage,
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
        let resources = context.resources();
        if self.steps.is_empty() {
            // The root selection IS the whole document: the tree path, whose parse cost is the answer's own cost.
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
            resources,
            self.coverage.attached_facts(),
        );
        let located_walk = walker.walk()?;
        let (builder, root) = self.materialize_walk(source, &located_walk, resources)?;
        let document = builder.finish(root, resources).map_err(map_data)?;
        let product = DocumentProduct::try_new(document, resources)?;
        self.publish_walk(&product, &located_walk)
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
            crate::walk::LocatedWalk::ImplicitTable { pieces } => {
                crate::lazy::build_implicit_table(source.bytes(), pieces, self.dialect, self.coverage, resources)
            }
            crate::walk::LocatedWalk::RangeValue { start, end, empty } => crate::lazy::build_range_value(
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
