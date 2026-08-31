//! Structured codec failures and their prose diagnostics.
//!
//! [`CodecFailureKind`] is closed. [`CodecFailureKind::RawNulByte`] is the only per-value recoverable kind. Sibling:
//! [`jqf_source::Diagnostic`].

use alloc::boxed::Box;

use jqf_data::{DataError, DataErrorClass};
use jqf_resource::{ControlError, ResourceError};
use jqf_source::{Label, Namespace, ResolvedSource, Severity, Span};

/// Allocation-free classification and structured payload for a codec failure.
///
/// [`Self::RawNulByte`] is the only per-value recoverable kind. Every other kind is terminal for a drive that consumes
/// this channel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodecFailureKind {
    /// Input violates the selected format or dialect.
    InvalidInput,
    /// A semantic value cannot be represented by the target.
    UnsupportedRepresentation,
    /// No physical route satisfies an exact requirement.
    RequirementMismatch,
    /// A provider or route identity does not belong to this carrier.
    ProviderRouteMismatch,
    /// A tag identity is invalid for the target codec.
    InvalidTag,
    /// Distinct stored tag identities collide in the target codec.
    CollidingTags,
    /// Request accounting rejected an operation.
    Resource(ResourceError),
    /// Host cancellation or deadline stopped the operation.
    Control(ControlError),
    /// Checked non-resource arithmetic overflowed.
    Overflow,
    /// Allocation outside a foundation tracked owner failed.
    AllocationFailure,
    /// An internal codec contract was violated.
    InternalContractViolation {
        /// Static invariant name.
        contract: &'static str,
    },
    /// A root string dumped with a NUL terminator contains a literal NUL byte.
    RawNulByte,
}

impl CodecFailureKind {
    /// The registry code for this machine-failure family — the binding contract's spine (jqf-resource's generated
    /// diagnostic-code registry). The engine's `EngineRunError::diagnostic_code` delegates here so the kind-to-code map
    /// lives with the kind.
    #[must_use]
    pub fn diagnostic_code(&self) -> u16 {
        use jqf_resource::diag::codes;
        match self {
            Self::InvalidInput => codes::MACHINE_INPUT,
            Self::UnsupportedRepresentation => codes::MACHINE_REPRESENTATION,
            Self::RequirementMismatch => codes::MACHINE_REQUIREMENT,
            Self::ProviderRouteMismatch => codes::MACHINE_ROUTE_MISMATCH,
            Self::InvalidTag => codes::MACHINE_INVALID_TAG,
            Self::CollidingTags => codes::MACHINE_COLLIDING_TAGS,
            Self::Resource(_) => codes::MACHINE_RESOURCE,
            Self::Control(ControlError::Cancelled) => codes::MACHINE_CANCELLED,
            Self::Control(ControlError::DeadlineExceeded) => codes::MACHINE_DEADLINE,
            Self::Control(ControlError::MemoryExceeded) => codes::MACHINE_MEMORY,
            Self::Overflow => codes::MACHINE_OVERFLOW,
            Self::AllocationFailure => codes::MACHINE_ALLOCATION,
            Self::InternalContractViolation { .. } => codes::MACHINE_INTERNAL_CONTRACT,
            Self::RawNulByte => codes::MACHINE_RAW_NUL,
        }
    }
}

/// Structured codec failure with an optional plain structured diagnostic (one carrier — `jqf_source::Diagnostic` —
/// for every producer; diagnostic memory is not request-accounted).
#[derive(Clone, Debug)]
pub struct CodecError {
    kind: CodecFailureKind,
    // Boxed to hold `CodecError` under the `result_large_err` lint's 128-byte DEFAULT THRESHOLD (not a pointer-size
    // claim): `kind` alone already embeds a `ResourceError` (~48-56 bytes), so an inline diagnostic would push the
    // carrier past it. The carrier is still the one plain type; the box is storage.
    diagnostic: Option<Box<jqf_source::Diagnostic>>,
}

impl CodecError {
    /// Creates a failure without allocating display text.
    #[must_use]
    pub const fn new(kind: CodecFailureKind) -> Self {
        Self { kind, diagnostic: None }
    }

    /// Attaches a structured diagnostic (already built fallibly by the caller, so attachment itself cannot fail).
    #[must_use]
    pub fn with_diagnostic(mut self, diagnostic: jqf_source::Diagnostic) -> Self {
        self.diagnostic = Some(Box::new(diagnostic));
        self
    }

    /// Failure classification.
    #[must_use]
    pub const fn kind(&self) -> CodecFailureKind {
        self.kind
    }

    /// Optional structured diagnostic.
    #[must_use]
    pub fn diagnostic(&self) -> Option<&jqf_source::Diagnostic> {
        self.diagnostic.as_deref()
    }
}

impl From<ResourceError> for CodecError {
    fn from(error: ResourceError) -> Self {
        Self::new(CodecFailureKind::Resource(error))
    }
}

impl From<ControlError> for CodecError {
    fn from(error: ControlError) -> Self {
        Self::new(CodecFailureKind::Control(error))
    }
}

impl From<jqf_resource::CooperativeError> for CodecError {
    fn from(error: jqf_resource::CooperativeError) -> Self {
        Self::new(match error {
            jqf_resource::CooperativeError::Control(error) => CodecFailureKind::Control(error),
            jqf_resource::CooperativeError::Memory(error) => CodecFailureKind::Resource(error),
        })
    }
}

/// Builds one source-aware reject diagnostic in a codec's namespace and attaches it to a bare `CodecError`, falling
/// back to the bare failure when a diagnostic allocation cannot be made (an unrepresentable document never gets worse
/// — the plain-carrier successor of the accounted refusal).
///
/// The one shared constructor behind every codec's source-aware `invalid` / `unsupported` helpers: the per-codec copies
/// differed only in the namespace constant, so the namespace is the parameter.
#[must_use]
pub fn diagnosed(
    kind: CodecFailureKind,
    namespace: Namespace,
    source: ResolvedSource<'_>,
    start: usize,
    end: usize,
    code: &'static str,
    message: &'static str,
) -> CodecError {
    let base = CodecError::new(kind);
    let Some(mut diagnostic) = jqf_source::Diagnostic::try_new(namespace.code(code), Severity::Error, message) else {
        return base;
    };
    let Some(source_record) =
        jqf_source::DiagnosticSource::try_new(source.source(), source.label(), source.base_offset())
    else {
        return base;
    };
    let Some(extended) = diagnostic.try_with_source(source_record) else {
        return base;
    };
    diagnostic = extended;
    // Reversed or overflowing coordinates: the caller's own kind, never an Overflow substitution, is what survives (the
    // accounted construction refused the same way).
    let Ok(span) = Span::try_from_usize(start, end) else {
        return base;
    };
    let Some(label) = Label::try_primary(source.source(), span, message) else {
        return base;
    };
    let Some(extended) = diagnostic.try_with_label(label) else {
        return base;
    };
    base.with_diagnostic(extended)
}

/// One shared `InternalContractViolation` constructor: the contract string is the only per-codec difference, so it is
/// the parameter.
#[must_use]
pub fn data_contract(contract: &'static str) -> CodecError {
    CodecError::new(CodecFailureKind::InternalContractViolation { contract })
}

/// Maps one document-construction [`DataError`] onto the codec failure vocabulary.
///
/// Classification follows [`DataError::class`]: host pressure passes through, budget stays budget, an unrepresentable
/// or cyclic graph is [`CodecFailureKind::UnsupportedRepresentation`], and absent/broken document contracts collapse
/// to the caller's `contract` string (the [`data_contract`] pattern). A future [`DataError`] variant that classifies as
/// [`DataErrorClass::Broken`] lands on that same contract rather than silently as corrupt input or host pressure.
#[must_use]
pub fn map_data(error: DataError, contract: &'static str) -> CodecError {
    match error.class() {
        DataErrorClass::Host => match error {
            DataError::Resource(error) => error.into(),
            DataError::Control(error) => error.into(),
            _ => CodecError::new(CodecFailureKind::InternalContractViolation { contract }),
        },
        DataErrorClass::Budget => match error {
            DataError::ArithmeticOverflow => CodecError::new(CodecFailureKind::Overflow),
            DataError::Allocation => CodecError::new(CodecFailureKind::AllocationFailure),
            _ => CodecError::new(CodecFailureKind::InternalContractViolation { contract }),
        },
        DataErrorClass::Unrepresentable => CodecError::new(CodecFailureKind::UnsupportedRepresentation),
        // Absent, Broken, and any future class fail closed as the caller's contract rather than as host pressure.
        _ => CodecError::new(CodecFailureKind::InternalContractViolation { contract }),
    }
}

/// Restates one nested span-materialization failure in the seam's vocabulary.
///
/// Resource and control failures pass through, same as [`map_data`]. A subtree that hit the memory ceiling must read as
/// the memory ceiling, not as a corrupt document. Only a re-read that rejects the span's own syntax reads as a corrupt
/// document; every other kind — a contract violation, a route or requirement mismatch, a tag failure — is a defect
/// in the materializer rather than in the bytes, and the span was accepted by the outer validating scan before it was
/// ever committed, so blaming the document would send a reader looking for a fault in their input that is not there.
#[must_use]
pub fn map_span_materialization_error(error: &CodecError) -> DataError {
    match error.kind() {
        CodecFailureKind::Resource(error) => DataError::Resource(error),
        CodecFailureKind::Control(error) => DataError::Control(error),
        CodecFailureKind::Overflow => DataError::ArithmeticOverflow,
        CodecFailureKind::AllocationFailure => DataError::Allocation,
        CodecFailureKind::InvalidInput => DataError::InvalidDocument,
        _ => DataError::ReaderFailed,
    }
}

/// Human-readable rendering of one failure KIND, and the ONLY sanctioned way to show a kind to a person.
///
/// `Debug` on this type is a Rust variant name (`UnsupportedRepresentation`, `InternalContractViolation { contract:
/// "…" }`); printing that on stderr leaks type syntax into a user diagnostic, which is exactly the defect this impl
/// exists to prevent (the same prose law `ResourceError`'s Display keeps in jqf-resource). Every arm is a sentence: no
/// variant name, no field name, no braces. Facades own the frame (`jqf: codec failed: …`) and any flag hint; the body
/// is here, beside the kind it renders, so the FFI's `MACHINE_SETUP` payload and the CLI's structured lines both read
/// it instead of each inventing a third wording.
///
/// The two host-flavored arms (a `Resource` note and the physical-memory refusal) stay facade-side: jqf-codec-core
/// cannot know the CLI's ceiling hint, so those arms delegate to the resource type's own prose and the caller supplies
/// the hint.
impl core::fmt::Display for CodecFailureKind {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidInput => formatter.write_str("the input does not match the selected format or dialect"),
            Self::UnsupportedRepresentation => {
                formatter.write_str("the value cannot be represented in the output format")
            }
            Self::RequirementMismatch => formatter.write_str("no codec route satisfies the exact request"),
            Self::ProviderRouteMismatch => {
                formatter.write_str("a codec route identity does not belong to the provider")
            }
            Self::InvalidTag => formatter.write_str("a tag identity is invalid for the target format"),
            Self::CollidingTags => formatter.write_str("distinct tag identities collide in the target format"),
            // The resource error's own Display is the prose law; the host hint (a ceiling flag) is the facade's to
            // append.
            Self::Resource(error) => write!(formatter, "{error}"),
            Self::Control(jqf_resource::ControlError::Cancelled) => formatter.write_str("the operation was cancelled"),
            Self::Control(jqf_resource::ControlError::DeadlineExceeded) => {
                formatter.write_str("the operation exceeded its deadline")
            }
            Self::Control(jqf_resource::ControlError::MemoryExceeded) => {
                formatter.write_str("the physical memory ceiling was exceeded")
            }
            Self::Overflow => formatter.write_str("checked arithmetic overflowed"),
            Self::AllocationFailure => formatter.write_str("allocation failed"),
            // The contract name is the diagnosis: it names the invariant the engine believes was broken, so it stays in
            // the message.
            Self::InternalContractViolation { contract } => {
                write!(formatter, "internal contract violation: {contract}")
            }
            Self::RawNulByte => formatter.write_str("cannot dump a string containing NUL with --raw-output0"),
        }
    }
}

impl core::fmt::Display for CodecError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "{}", self.kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::vec::Vec;

    /// Every renderable kind, so the prose gate below sweeps the whole enum rather than the arms someone remembered.
    fn every_kind() -> Vec<CodecFailureKind> {
        use jqf_resource::{ControlError, ResourceError, ResourceLimit};
        let resource = ResourceError::LimitExceeded {
            limit_kind: ResourceLimit::MemoryBytes,
            limit: 104_857_600,
            current: 92_812_697,
            requested_delta: 25_165_872,
        };
        Vec::from([
            CodecFailureKind::InvalidInput,
            CodecFailureKind::UnsupportedRepresentation,
            CodecFailureKind::RequirementMismatch,
            CodecFailureKind::ProviderRouteMismatch,
            CodecFailureKind::InvalidTag,
            CodecFailureKind::CollidingTags,
            CodecFailureKind::Resource(resource),
            CodecFailureKind::Control(ControlError::Cancelled),
            CodecFailureKind::Control(ControlError::DeadlineExceeded),
            CodecFailureKind::Control(ControlError::MemoryExceeded),
            CodecFailureKind::Overflow,
            CodecFailureKind::AllocationFailure,
            CodecFailureKind::InternalContractViolation {
                contract: "engine navigation over a valid located document",
            },
            CodecFailureKind::RawNulByte,
        ])
    }

    /// The standing guard against a `Debug` rendering reaching a person: no braces, no `::` paths, no variant name, and
    /// no field name anywhere in the text — the prose law, the same guard `ResourceError`'s own suite keeps. A new
    /// variant whose `Display` arm is copied from `Debug` fails here rather than on a user's terminal.
    #[test]
    fn every_rendering_is_prose_not_rust_syntax() {
        let banned = [
            "{",
            "}",
            "::",
            "InvalidInput",
            "UnsupportedRepresentation",
            "RequirementMismatch",
            "ProviderRouteMismatch",
            "InvalidTag",
            "CollidingTags",
            "LimitExceeded",
            "limit_kind",
            "MemoryBytes",
            "Cancelled",
            "DeadlineExceeded",
            "MemoryExceeded",
            "Overflow",
            "AllocationFailure",
            "InternalContractViolation",
            "RawNulByte",
        ];
        for kind in every_kind() {
            let rendered = format!("{kind}");
            assert!(!rendered.is_empty(), "a kind must render to something");
            for needle in banned {
                assert!(
                    !rendered.contains(needle),
                    "rendered codec failure leaks Rust syntax {needle:?}: {rendered}"
                );
            }
        }
    }

    /// The two arms whose wording is load-bearing for the FFI's `MACHINE_SETUP` payload and the CLI's worded failures:
    /// the parse/compile boundary and the internal contract keep their diagnosis in the sentence.
    #[test]
    fn the_contract_name_stays_in_the_sentence() {
        let rendered = format!(
            "{}",
            CodecFailureKind::InternalContractViolation {
                contract: "engine navigation over a valid located document",
            }
        );
        assert_eq!(
            rendered,
            "internal contract violation: engine navigation over a valid located document"
        );
    }

    /// A1: when diagnostic construction is refused, the caller's own kind — never the construction/attachment error
    /// — is what survives (an unrepresentable document never gets worse). Forced deterministically here with a
    /// reversed span, which makes `make()` fail at its Span arm before any allocation could be refused.
    #[test]
    fn diagnosed_construction_refusal_keeps_the_caller_kind() {
        let source = test_source(b"{}");
        let error = diagnosed(
            CodecFailureKind::UnsupportedRepresentation,
            Namespace::new("test"),
            source,
            2,
            1, // reversed: Span::try_from_usize refuses
            "test.unsupported",
            "cannot represent this value",
        );
        assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
        assert!(error.diagnostic().is_none());
    }

    /// The happy path still attaches the diagnostic under the caller's kind.
    #[test]
    fn diagnosed_success_attaches_the_diagnostic() {
        let source = test_source(b"{}");
        let error = diagnosed(
            CodecFailureKind::InvalidInput,
            Namespace::new("test"),
            source,
            0,
            1,
            "test.invalid",
            "bad input",
        );
        assert_eq!(error.kind(), CodecFailureKind::InvalidInput);
        assert!(error.diagnostic().is_some());
    }

    fn test_source(bytes: &[u8]) -> ResolvedSource<'_> {
        use jqf_source::{SourceId, SourceKind, SourceRef};
        ResolvedSource::new(SourceRef::new(SourceId::new(1), SourceKind::Input), "test", bytes, 0)
    }

    #[test]
    fn map_data_classifies_unrepresentable_budget_and_absent() {
        use jqf_data::DocumentCapability;

        assert_eq!(
            map_data(DataError::UnrepresentableSemantic, "test contract").kind(),
            CodecFailureKind::UnsupportedRepresentation
        );
        assert_eq!(
            map_data(DataError::CyclicSemanticGraph, "test contract").kind(),
            CodecFailureKind::UnsupportedRepresentation
        );
        assert_eq!(
            map_data(DataError::Allocation, "test contract").kind(),
            CodecFailureKind::AllocationFailure
        );
        assert_eq!(
            map_data(DataError::ArithmeticOverflow, "test contract").kind(),
            CodecFailureKind::Overflow
        );
        assert!(matches!(
            map_data(
                DataError::CapabilityUnavailable {
                    capability: DocumentCapability::AttachedFacts,
                },
                "test contract"
            )
            .kind(),
            CodecFailureKind::InternalContractViolation {
                contract: "test contract"
            }
        ));
        assert!(matches!(
            map_data(DataError::InvalidDocument, "test contract").kind(),
            CodecFailureKind::InternalContractViolation {
                contract: "test contract"
            }
        ));
    }
}
