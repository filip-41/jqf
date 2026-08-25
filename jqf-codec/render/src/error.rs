//! Render codec failure construction.
//!
//! The render package is output-only: it has no input source to point a diagnostic at. Shape and cap failures still
//! carry a message through a source-less structured diagnostic so the CLI renders prose instead of a bare kind.

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_source::{Namespace, Severity};

const RENDER: Namespace = Namespace::new("render");

/// A value the renderer cannot present: the wrong shape for the dialect, or a cell/layout/cap law violated before any
/// frame could be published. The per-failure `code` is what distinguishes a shape refusal from a cap refusal; both
/// publish no frame and both carry the same kind.
pub(crate) fn unsupported(code: &'static str, message: &'static str) -> CodecError {
    let base = CodecError::new(CodecFailureKind::UnsupportedRepresentation);
    let Some(diagnostic) = jqf_source::Diagnostic::try_new(RENDER.code(code), Severity::Error, message) else {
        return base;
    };
    base.with_diagnostic(diagnostic)
}

/// A refusal whose prose must name a dynamic path or value (the shell renderer's key/collision/value laws).
/// [`unsupported`] is the static-message twin; on diagnostic-construction refusal the bare failure survives, exactly as
/// there.
pub(crate) fn unsupported_owned(code: &'static str, message: &str) -> CodecError {
    let base = CodecError::new(CodecFailureKind::UnsupportedRepresentation);
    let Some(diagnostic) = jqf_source::Diagnostic::try_new(RENDER.code(code), Severity::Error, message) else {
        return base;
    };
    base.with_diagnostic(diagnostic)
}

/// A renderer-internal invariant was violated.
pub(crate) fn contract(message: &'static str) -> CodecError {
    CodecError::new(CodecFailureKind::InternalContractViolation { contract: message })
}

/// A renderer-scoped allocation failure.
pub(crate) fn allocation() -> CodecError {
    CodecError::new(CodecFailureKind::AllocationFailure)
}
