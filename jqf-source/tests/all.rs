//! Every integration test for this crate, compiled as a single binary. Files stay separated by contract area; only this
//! target is built.

#[path = "cases/diagnostic.rs"]
mod diagnostic;
#[path = "cases/source.rs"]
mod source;
#[path = "cases/span.rs"]
mod span;
