//! Every integration test for this crate, compiled as a single binary. Files stay separated by contract area; only this
//! target is built.

#[path = "cases/associativity.rs"]
mod associativity;
mod common;
#[path = "cases/conformance.rs"]
mod conformance;
#[path = "cases/diagnostic.rs"]
mod diagnostic;
#[path = "cases/input.rs"]
mod input;
#[path = "cases/lexer.rs"]
mod lexer;
#[path = "cases/limits.rs"]
mod limits;
#[path = "cases/operator_authority.rs"]
mod operator_authority;
#[path = "cases/parser.rs"]
mod parser;
#[path = "cases/recovery.rs"]
mod recovery;
#[path = "cases/robustness.rs"]
mod robustness;
#[path = "cases/source_binding.rs"]
mod source_binding;
#[path = "cases/string_decode.rs"]
mod string_decode;
#[path = "cases/token.rs"]
mod token;
#[path = "cases/traversal.rs"]
mod traversal;
