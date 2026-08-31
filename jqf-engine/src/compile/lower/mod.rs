//! AST lowering into the pre-fusion program arena, plus the
//! [`try_lower_program_unit`] entry that folds module preparation into
//! program-unit lowering.

#![allow(clippy::wildcard_imports)]

mod bind;
mod call;
mod context;
mod defs;
mod expr;
mod modules;
mod postfix;

pub(super) use bind::*;
pub(super) use call::*;
pub(super) use context::*;
pub(super) use defs::*;
pub(super) use expr::*;
pub(super) use modules::*;
pub(super) use postfix::*;

use super::EngineCompileError;
pub(crate) use super::prelude::*;
pub(crate) use super::try_copy_str;
pub(crate) use super::{ParseRejection, UnsupportedConstruct};

/// Options that change lowering behavior without changing the parsed source.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LowerOptions {
    runtime_index: bool,
}

impl LowerOptions {
    /// Ordinary compile: `$index` is undefined unless bound in the program.
    #[must_use]
    pub(crate) const fn new() -> Self {
        Self { runtime_index: false }
    }

    /// Split-expression compile: pre-bind `$index` to a runtime slot.
    #[must_use]
    pub(crate) const fn split_exp() -> Self {
        Self { runtime_index: true }
    }

    #[must_use]
    pub(crate) const fn runtime_index(self) -> bool {
        self.runtime_index
    }
}

/// Inputs for [`try_lower_program_unit`].
pub(crate) struct LowerRequest<'ast, 'resources> {
    preludes: &'ast [(&'ast Expr, &'ast SyntaxSource<'ast>)],
    unit: &'ast jqf_syntax::SourceUnit,
    source: &'ast SyntaxSource<'ast>,
    cli_vars: &'ast [(String, Value)],
    options: LowerOptions,
    resources: &'resources ResourceContext<'resources>,
}

impl<'ast, 'resources> LowerRequest<'ast, 'resources> {
    pub(crate) fn new(
        preludes: &'ast [(&'ast Expr, &'ast SyntaxSource<'ast>)],
        unit: &'ast jqf_syntax::SourceUnit,
        source: &'ast SyntaxSource<'ast>,
        cli_vars: &'ast [(String, Value)],
        options: LowerOptions,
        resources: &'resources ResourceContext<'resources>,
    ) -> Self {
        Self {
            preludes,
            unit,
            source,
            cli_vars,
            options,
            resources,
        }
    }
}

/// What [`try_lower_program_unit`] returns before fuse/analyze.
pub(crate) struct LowerOutput {
    nodes: Vec<ProgramNode>,
    root: ProgramNodeId,
    slots: u32,
    callables: Vec<CallableDef>,
    uses_inputs_cursor: bool,
    runtime_index_slot: Option<u32>,
}

impl LowerOutput {
    fn from_lowered(lowered: Lowered) -> Self {
        let (nodes, root, slots, callables, uses_inputs_cursor, runtime_index_slot) = lowered;
        Self {
            nodes,
            root,
            slots,
            callables,
            uses_inputs_cursor,
            runtime_index_slot,
        }
    }

    pub(crate) fn into_program_parts(
        self,
    ) -> (
        Vec<ProgramNode>,
        ProgramNodeId,
        u32,
        Vec<CallableDef>,
        bool,
        Option<u32>,
    ) {
        (
            self.nodes,
            self.root,
            self.slots,
            self.callables,
            self.uses_inputs_cursor,
            self.runtime_index_slot,
        )
    }
}

/// Prepares included modules, binds them, and lowers one program unit.
pub(crate) fn try_lower_program_unit(request: &LowerRequest<'_, '_>) -> Result<LowerOutput, EngineCompileError> {
    let mut prepared = Vec::new();
    let mut seen = BTreeSet::new();
    prepare_included_modules(
        request.unit,
        request.source,
        request.resources,
        None,
        &mut prepared,
        &mut seen,
    )?;
    let module_bounds: Vec<BoundModule<'_>> = prepared.iter().map(|module| BoundModule { module }).collect();
    let lowered = lower_program_unit(
        request.preludes,
        request.unit,
        request.source,
        &module_bounds,
        request.cli_vars,
        request.resources,
        request.options.runtime_index(),
    )?;
    Ok(LowerOutput::from_lowered(lowered))
}

/// Lowers one expression for tests: bypasses [`try_lower_program_unit`]'s
/// module/prepare path and the production prelude/CLI binding surface. Returns
/// the same pre-fusion [`LowerOutput`] shape so lowering tests can snapshot the
/// arena before fuse/analyze.
#[cfg(test)]
pub(crate) fn try_lower_expr<'ast>(
    expr: &'ast Expr,
    source: &SyntaxSource<'ast>,
    resources: &ResourceContext<'_>,
) -> Result<LowerOutput, EngineCompileError> {
    let lowered = lower(expr, source, resources)?;
    Ok(LowerOutput::from_lowered(lowered))
}

/// Like [`try_lower_expr`], but enables the split lane's `$index` resolution.
#[cfg(test)]
pub(crate) fn try_lower_expr_split<'ast>(
    expr: &'ast Expr,
    source: &SyntaxSource<'ast>,
    resources: &ResourceContext<'_>,
) -> Result<LowerOutput, EngineCompileError> {
    let lowered = lower_with_prelude(&[], expr, source, &[], resources, true)?;
    Ok(LowerOutput::from_lowered(lowered))
}
