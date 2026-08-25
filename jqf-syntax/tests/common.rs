//! Shared parse helpers for the integration-test binary.
//!
//! `all.rs` compiles every test file into ONE target, so the wrapper trio and the test source identity live here once
//! instead of in every case file. Each file keeps its own domain-specific helpers; only the common entry plumbing is
//! shared.

use jqf_source::{SourceId, SourceKind, SourceRef};
use jqf_syntax::{Expr, Parse, SourceUnit};

/// Test source identity for every file's parse calls.
pub fn source() -> SourceRef {
    SourceRef::new(SourceId::new(23), SourceKind::Query)
}

/// Parses one query, panicking only when the test source cannot be represented by compact spans (never on a grammar
/// diagnostic).
pub fn parse_query(source: SourceRef, text: &str) -> Parse<Expr> {
    jqf_syntax::parse_query(source, text).expect("test source fits compact syntax spans")
}

/// Parses one program source unit.
pub fn parse_program(source: SourceRef, text: &str) -> Parse<SourceUnit> {
    jqf_syntax::parse_program(source, text).expect("test source fits compact syntax spans")
}

/// Parses one library source unit.
pub fn parse_library(source: SourceRef, text: &str) -> Parse<SourceUnit> {
    jqf_syntax::parse_library(source, text).expect("test source fits compact syntax spans")
}
