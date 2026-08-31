//! Compile pipeline: gate, preludes, parse, bind, lower, transform, analyze, finish.
//!
//! Stage order is fixed: [`super::prelude_gate::scan_prelude_gate`] scans user
//! source, optional embedded preludes load when needed, then parse → bind →
//! lower → [`super::transform`] → analyze → [`CompiledProgram::finish`]. Gate
//! through transform are fallible; analyze and finish do not surface
//! [`EngineCompileError`].
//!
//! [`try_compile_program`] is the public entry.

use super::error::EngineCompileError;
use super::lower::{LowerOptions, LowerRequest, try_lower_program_unit};
use super::parse::{bind_syntax, into_valid_syntax, parse_program_input};
use super::prelude::*;
use super::prelude_gate::scan_prelude_gate;
use super::program::CompiledProgram;
use super::transform::transform_lowered;

/// CLI bindings and compile-lane flags for lowering.
#[derive(Clone, Copy, Debug)]
pub struct CompileOptions<'a> {
    /// `--arg` / `--argjson` bindings in scope during lowering.
    pub cli_vars: &'a [(String, Value)],
    /// Compile the split-expression lane: pre-bind `$index` instead of rejecting it.
    pub split_exp: bool,
    /// Human-facing source label for bind and `$__loc__`.
    pub source_label: &'a str,
}

impl Default for CompileOptions<'_> {
    fn default() -> Self {
        Self {
            cli_vars: &[],
            split_exp: false,
            source_label: "<top-level>",
        }
    }
}

impl CompileOptions<'_> {
    /// Ordinary compile with no CLI bindings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Split-expression compile with no CLI bindings.
    #[must_use]
    pub fn split_exp() -> Self {
        Self {
            split_exp: true,
            ..Self::default()
        }
    }
}

/// Parses and compiles one program into a [`CompiledProgram`].
///
/// # Errors
///
/// See [`EngineCompileError`]: input and parse failures, unsupported
/// constructs, name-resolution failures (undefined calls, variables, labels,
/// engine bindings), and [`EngineCompileError::Resource`] when the request
/// ledger refuses an arena allocation.
pub fn try_compile_program(
    source: &str,
    policy: CodecRequirementPolicy,
    options: CompileOptions<'_>,
    resources: &ResourceContext<'_>,
) -> Result<CompiledProgram, EngineCompileError> {
    compile(source, policy, options, resources)
}

fn compile(
    source: &str,
    policy: CodecRequirementPolicy,
    options: CompileOptions<'_>,
    resources: &ResourceContext<'_>,
) -> Result<CompiledProgram, EngineCompileError> {
    let source_ref = SourceRef::new(SourceId::new(0), SourceKind::Query);
    let prelude_gate = scan_prelude_gate(source);
    let preludes = {
        let mut out = Vec::new();
        if prelude_gate.needs_stdlib {
            let (root, source) = super::prelude_gate::stdlib_prelude()?;
            out.push((root, source));
        }
        if prelude_gate.needs_extension {
            let (root, source) = super::prelude_gate::extension_prelude()?;
            out.push((root, source));
        }
        out
    };
    let lowered = {
        // Parse: lex/parse user source as a program unit.
        let parse = parse_program_input(source_ref, source)?;
        // Valid: reject recoverable parse debris before bind.
        let syntax = into_valid_syntax(parse)?;
        // Bind: attach span text for lowering and `$__loc__`.
        let bound = bind_syntax(&syntax, source_ref, options.source_label, source)?;
        let lower_options = if options.split_exp {
            LowerOptions::split_exp()
        } else {
            LowerOptions::new()
        };
        if options.split_exp && !options.cli_vars.is_empty() {
            return Err(EngineCompileError::SplitExpWithCliVars);
        }
        let cli_vars = options.cli_vars;
        try_lower_program_unit(&LowerRequest::new(
            &preludes,
            bound.root(),
            bound.source(),
            cli_vars,
            lower_options,
            resources,
        ))?
    };
    let (mut nodes, root, mut slots, callables, uses_inputs_cursor, runtime_index_slot) = lowered.into_program_parts();
    let (nodes, root) = transform_lowered(&mut nodes, root, &mut slots, &callables)?;
    let split = analyze(&nodes, root);
    let program = Program::new(nodes, root, split, slots);
    Ok(CompiledProgram::finish(
        program,
        policy,
        uses_inputs_cursor,
        runtime_index_slot,
    ))
}
