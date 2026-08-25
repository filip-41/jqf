//! Output-only presentation renderers (`render` format).
//!
//! This package registers NO input provider, format detection, source edit, or round-trip claim: it is the OUTPUT side
//! of the presentation vertical. Eight base renderer profiles are registered — plain text, GFM markdown table, HTML
//! table fragment, ASCII grid table, tree, terminal-safe styled text, POSIX `sh` assignments, and a plain-ASCII
//! frequency histogram — each composed with the layout/width/header option law. The registration declares the RECORD
//! route: every renderer is atomic per item, so a record request renders one frame per record. A request binds one
//! renderer ID as the DIALECT and the composition via [`RenderEncodeOptions`].
//!
//! Publication is one complete frame per semantic input item; the facade appends the single final LF and no BOM, so
//! zero items publish zero frames. A table renderer is atomic per table: one item is one table, drained fully before
//! any of its frame is published, and a cap failure emits no frame.

#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(
    clippy::missing_errors_doc,
    reason = "fallible APIs use the codec crate's closed structured error vocabulary"
)]

extern crate alloc;

mod atom;
mod encode;
mod error;
mod hist;
mod options;
mod plain;
mod scalar;
mod shell;
mod spans;
mod table;
mod tree;

use jqf_codec_core::{
    CodecDescriptor, CodecOperations, CodecRegistration, EncoderFactoryRecord, ItemByteOwner, RegistrationError,
    RouteCapability,
};
use jqf_data::{DialectIdRef, FormatIdRef};

pub use encode::ENCODE_PHYSICAL_ROUTE_ID;
pub use options::{
    FORMAT_ID, GFM_TABLE_DIALECT_ID, GRID_TABLE_DIALECT_ID, HIST_DIALECT_ID, HTML_TABLE_DIALECT_ID, HeaderPolicy,
    PLAIN_DIALECT_ID, RenderEncodeOptions, SHELL_DIALECT_ID, TERMINAL_DIALECT_ID, TREE_DIALECT_ID, TerminalShape,
    WidthProfile,
};

/// The deepest value any renderer walks: past this ceiling a renderer refuses by name rather than recurse without bound
/// (the tree printer's 10,000-level ceiling, shared by every recursive walk in the crate).
pub(crate) const MAX_NESTING_DEPTH: usize = 10_000;

const FORMAT: FormatIdRef<'static> = FormatIdRef::from_static(FORMAT_ID);
const DIALECTS: [DialectIdRef<'static>; 8] = [
    DialectIdRef::from_static(PLAIN_DIALECT_ID),
    DialectIdRef::from_static(GFM_TABLE_DIALECT_ID),
    DialectIdRef::from_static(HTML_TABLE_DIALECT_ID),
    DialectIdRef::from_static(GRID_TABLE_DIALECT_ID),
    DialectIdRef::from_static(TREE_DIALECT_ID),
    DialectIdRef::from_static(TERMINAL_DIALECT_ID),
    DialectIdRef::from_static(SHELL_DIALECT_ID),
    DialectIdRef::from_static(HIST_DIALECT_ID),
];

/// The CLI-facing routes the render registration serves: the RECORD route (each record renders to its own frame, so
/// CSV/NDJSON/TSV/json-seq input aggregates into markdown/table/tree/shell output end to end) and ADJACENT VALUES (a
/// multi-item run publishes one frame per item — without the declaration the publication layer fences render to one
/// document per run, which is exactly the refusal the record route must not hit on a multi-record stream).
const ROUTES: [RouteCapability; 2] = [RouteCapability::Record, RouteCapability::AdjacentValues];

/// Registers the `render` format's ENCODE side: eight output profiles, no input registration, no tag validation.
/// `CodecOperations` advertises encode only.
pub fn registration() -> Result<CodecRegistration<'static>, RegistrationError> {
    CodecRegistration::try_new(
        CodecDescriptor::new(
            FORMAT,
            &DIALECTS,
            CodecOperations::new(false, true, false),
            &ROUTES,
            // Output-only: no input format, so no detection extension.
            &[],
            // Every renderer's frame carries its interior LFs; the facade appends the single final LF.
            &[ItemByteOwner::Facade; 8],
            &[],
            // No insignificant inter-value bytes: every byte reaches the decoder.
            &[],
        ),
        None,
        Some(EncoderFactoryRecord::new(encode::create_factory)),
        None,
        None,
    )
}

/// Compiles the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
