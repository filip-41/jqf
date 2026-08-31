//! Parse → valid → bind error mapping shared by the compile pipeline.

#[allow(clippy::wildcard_imports)]
use super::*;

pub(crate) fn parse_program_input(
    source_ref: SourceRef,
    text: &str,
) -> Result<jqf_syntax::Parse<jqf_syntax::SourceUnit>, EngineCompileError> {
    parse_program(source_ref, text).map_err(EngineCompileError::Input)
}

pub(crate) fn parse_library_input(
    source_ref: SourceRef,
    text: &str,
) -> Result<jqf_syntax::Parse<jqf_syntax::SourceUnit>, EngineCompileError> {
    jqf_syntax::parse_library(source_ref, text).map_err(EngineCompileError::Input)
}

pub(crate) fn parse_query_input(
    source_ref: SourceRef,
    text: &str,
) -> Result<jqf_syntax::Parse<Expr>, EngineCompileError> {
    parse_query(source_ref, text).map_err(EngineCompileError::Input)
}

pub(crate) fn into_valid_syntax<T>(
    parse: jqf_syntax::Parse<T>,
) -> Result<jqf_syntax::ParsedSyntax<T>, EngineCompileError> {
    parse
        .into_valid_syntax()
        .map_err(|diagnostics| EngineCompileError::Parse(ParseRejection::from_diagnostics(&diagnostics)))
}

pub(crate) fn bind_syntax<'tree, 'source, T>(
    syntax: &'tree jqf_syntax::ParsedSyntax<T>,
    source_ref: SourceRef,
    label: &'source str,
    text: &'source str,
) -> Result<jqf_syntax::BoundSyntax<'tree, 'source, T>, EngineCompileError> {
    let resolved = ResolvedSource::new(source_ref, label, text.as_bytes(), 0);
    syntax
        .bind(resolved)
        .map_err(|error| EngineCompileError::Parse(ParseRejection::from_bind(error)))
}

pub(crate) fn recovered_syntax_error() -> EngineCompileError {
    debug_assert!(false, "valid syntax must not contain Error nodes");
    EngineCompileError::Parse(ParseRejection::internal("a recovered syntax error"))
}
