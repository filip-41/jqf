//! Decode and encode construction policy: diagnostics, validation, preservation.
//!
//! Sibling: [`crate::registration`] for the factories that consume these.

use core::any::Any;
use jqf_data::{DialectId, FormatId};

use crate::{CodecError, CodecFailureKind};

/// Decoder validation policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationMode {
    /// Enforce the target dialect completely before publishing a product.
    Strict,
    /// Accept only codec-declared recoverable deviations and report them.
    Recover,
}

/// Structured diagnostic retention policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticPolicy {
    /// Retain no diagnostics at all: no sink is installed and nothing is produced (the zero-cost policy).
    Off,
    /// Retain only diagnostics which make the request fail.
    ErrorsOnly,
    /// Retain all codec-produced structured diagnostics.
    All,
}

/// Preservation evidence requested from an encoder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreservationRequest {
    /// No preservation report is required.
    None,
    /// Produce the codec's complete supported preservation report.
    Report,
}

/// Policy used to construct a source-bound decoder provider.
#[derive(Clone, Copy, Debug)]
pub struct DecodeRequest<'options> {
    /// Validation policy.
    pub validation: ValidationMode,
    /// Diagnostic retention policy.
    pub diagnostics: DiagnosticPolicy,
    /// The requested input dialect: the registration's decoder factory dispatches on it — one registration per codec
    /// serves every dialect it registers, and a codec whose decoder differs by input dialect (YAML's schema ladder)
    /// reads the dialect here instead of registering one factory per dialect.
    pub dialect: &'options DialectId,
    /// Optional format-specific options: the codec's own concrete options struct, borrowed as `&dyn Any`: a factory
    /// downcasts once to its own type, and a mismatch is a caller bug at a typed boundary, not a runtime-validated
    /// identity. Absence selects the codec's declared defaults.
    pub options: Option<&'options (dyn Any + Send + Sync)>,
    /// Opts into partial-input consumption: a codec that supports it accepts non-whitespace bytes after a complete root
    /// value instead of rejecting them as trailing content, and reports the exact consumed offset via
    /// [`crate::AccessReport::consumed_offset`] so the caller can decode the remainder as another adjacent value (e.g.
    /// NDJSON / space-separated JSON texts on one stream). Default (`false`) preserves the strict single-document
    /// contract: any codec that does not support this mode ignores the flag and keeps rejecting trailing content.
    pub allow_adjacent_values: bool,
    /// The byte set the sequence drives skip as insignificant whitespace between adjacent complete values when
    /// [`Self::allow_adjacent_values`] is enabled. Empty by default: every byte reaches the decoder. A codec whose
    /// grammar has insignificant inter-value whitespace (JSON, NDJSON, json-seq) passes that slice on the request; it
    /// does not live in this crate.
    pub value_separator: &'options [u8],
}

impl DecodeRequest<'_> {
    /// Front-door guard shared by every option-free decoder factory: the request must carry no options (defaults only)
    /// and name strict validation. A single-document codec that additionally refuses the adjacent-value contract keeps
    /// that check as its own line.
    ///
    /// # Errors
    ///
    /// Returns `RequirementMismatch` when the request names either unsupported policy.
    pub fn expect_strict_defaults(&self) -> Result<(), CodecError> {
        if self.options.is_some() || self.validation != ValidationMode::Strict {
            return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
        }
        Ok(())
    }
}

/// Policy used to construct a target-bound encoder.
#[derive(Clone, Copy, Debug)]
pub struct EncodeRequest<'target, 'options> {
    /// Target format.
    pub format: &'target FormatId,
    /// Target dialect.
    pub dialect: &'target DialectId,
    /// Diagnostic retention policy.
    pub diagnostics: DiagnosticPolicy,
    /// Preservation evidence requested by the SDK.
    pub preservation: PreservationRequest,
    /// Optional format-specific options: the codec's own concrete options struct, borrowed as `&dyn Any`. Absence
    /// selects codec defaults.
    pub options: Option<&'options (dyn Any + Send + Sync)>,
}

impl EncodeRequest<'_, '_> {
    /// Target guard shared by the encoder and tag-validator factories: the request must name this codec's format and
    /// one of the dialects the factory serves. A factory registered under several dialects passes them all; each
    /// request names exactly one.
    ///
    /// # Errors
    ///
    /// Returns `RequirementMismatch` when the target names another codec's profile.
    pub fn expect_target(&self, format: &str, dialects: &[&str]) -> Result<(), CodecError> {
        if self.format.as_str() != format || !dialects.contains(&self.dialect.as_str()) {
            return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
        }
        Ok(())
    }
}
