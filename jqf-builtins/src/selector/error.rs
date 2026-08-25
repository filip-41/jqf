//! The closed selector error vocabulary.

use alloc::string::String;

/// One selector failure, closed over the seam's own contract classes.
///
/// Compile errors carry the message and the byte offset; run-time errors carry the class and, where relevant, the
/// budget or format facts. The engine maps this vocabulary onto its catchable error surface; nothing here is a panic
/// and nothing here leaks a codec name.
#[derive(Debug)]
pub enum SelectorError {
    /// The selector text is outside the language's closed grammar. `offset` is the byte offset of the offending
    /// construct.
    Compile {
        /// Human-readable reason, naming the construct and the law it broke.
        message: String,
        /// Byte offset of the offending construct.
        offset: usize,
    },
    /// A run-time budget ceiling was exceeded.
    Budget {
        /// Which budget (`candidate tests`, `walk steps`, `results`).
        what: &'static str,
    },
    /// The selector language serves one format; the document is another.
    FormatMismatch {
        /// The language's stable identity text.
        language: &'static str,
        /// The document's format identity text.
        format: String,
    },
    /// The document's schema does not carry the language's markup fact roles, so it has no element authority to select
    /// from.
    NotMarkup,
    /// `html.css@1` requires the complete recovered document mode in its input authority; a document without the mode
    /// fact makes the selector route ineligible.
    MissingModeAuthority,
    /// An allocation was refused by the request ledger.
    Allocation,
    /// A request control stop (the cooperative cancellation class).
    Control,
    /// An internal invariant of the seam was violated over a valid document.
    Internal {
        /// The violated contract.
        contract: &'static str,
    },
}

impl From<jqf_resource::ControlError> for SelectorError {
    fn from(_error: jqf_resource::ControlError) -> Self {
        SelectorError::Control
    }
}

impl From<jqf_resource::CooperativeError> for SelectorError {
    fn from(error: jqf_resource::CooperativeError) -> Self {
        match error {
            jqf_resource::CooperativeError::Control(error) => error.into(),
            jqf_resource::CooperativeError::Memory(error) => error.into(),
        }
    }
}

impl From<jqf_resource::ResourceError> for SelectorError {
    fn from(error: jqf_resource::ResourceError) -> Self {
        match error {
            jqf_resource::ResourceError::LimitExceeded { .. }
            | jqf_resource::ResourceError::ArithmeticOverflow
            | jqf_resource::ResourceError::AllocationFailed
            | jqf_resource::ResourceError::OutputPermitExceeded { .. }
            | jqf_resource::ResourceError::AccountingInvariantViolation
            | jqf_resource::ResourceError::HostFailure { .. }
            | jqf_resource::ResourceError::RecursionLimit { .. } => SelectorError::Allocation,
        }
    }
}
