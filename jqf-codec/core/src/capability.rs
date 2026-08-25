//! Physical route capability bundles: footprint, result kind, demand, guarantees.
//!
//! One bundle per advertised route. Mixed result kinds in one bundle are unrepresentable. Sibling:
//! [`crate::descriptor`].

use crate::{CodecDemand, DiagnosticPolicy, ValidationMode};

/// Structural family accepted by an executable route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessFootprintKind {
    /// Complete semantic document input.
    Whole,
    /// One canonical exact path.
    Exact,
}

/// Authority returned by an access request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessResultKind {
    /// Complete authoritative document.
    CompleteDocument,
    /// One authority-retaining exact observation.
    Located,
    /// An ORDERED SEQUENCE of physically framed records over one retained source — the `jqf.record-stream@1` (see
    /// [`crate::record`]).
    ///
    /// It is the only result kind that is NOT an access observation: a record stream is opened through
    /// [`crate::ErasedRecordStreamProvider`], not through the access binder, and it delivers framed BYTE RANGES rather
    /// than semantic products. Each record's payload is then decoded through the payload codec's ordinary access
    /// ladder, which is what lets a record route reuse the recycled session and shared schema prototype the
    /// adjacent-value path already owns instead of building a second ladder beside it. It therefore has no core
    /// fallback adapter and can never be composed into an access requirement: a route advertising it is advertised in a
    /// record inventory, never in an access one.
    RecordStream,
}

/// Non-structural guarantees shared by requirements and executable bundles.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccessGuarantees {
    validation: ValidationMode,
    diagnostics: DiagnosticPolicy,
}
impl AccessGuarantees {
    /// Creates one exact guarantee conjunction.
    #[must_use]
    pub const fn new(validation: ValidationMode, diagnostics: DiagnosticPolicy) -> Self {
        Self {
            validation,
            diagnostics,
        }
    }
    /// The strict-validation conjunction every access provider wants.
    #[must_use]
    pub const fn strict(diagnostics: DiagnosticPolicy) -> Self {
        Self {
            validation: ValidationMode::Strict,
            diagnostics,
        }
    }
    /// Validation guarantee.
    #[must_use]
    pub const fn validation(self) -> ValidationMode {
        self.validation
    }
    /// Diagnostic guarantee.
    #[must_use]
    pub const fn diagnostics(self) -> DiagnosticPolicy {
        self.diagnostics
    }
}

/// One CLI-facing route a `(format, dialect)` pair can serve.
///
/// The declaration gates OUTPUT lanes as well as input lanes: a capability row is read on both sides of a request.
/// [`RouteCapability::Record`] marks the formats that take part in the record stream as OUTPUT targets too (plain JSON
/// advertises record output with no record decode), and the render registration's [`RouteCapability::AdjacentValues`]
/// row is what lets its multi-item publication lane open at all.
///
/// Which routes a codec DECLARES for its registrations, so the CLI reads its route facts from the codec instead of
/// re-declaring them as hand-written `match` arms. The declaration lives on the [`crate::CodecDescriptor`], and the CLI
/// consumes it through its catalog.
///
/// The access-route facts the access provider itself advertises ([`crate::InputProvider::route_descriptions`]) are NOT
/// re-declared here: the physical route identity is DERIVED from (format, kind, specialization) instead. The remaining
/// variants are the CLI input-model facts no provider route carries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteCapability {
    /// The record route: the format participates in the physical record stream (NDJSON / json-seq / CSV as record
    /// INPUT; JSON, NDJSON, json-seq, and CSV as record OUTPUT).
    Record,
    /// The source is a stream of adjacent complete texts (RFC 8259 texts, `---`-separated YAML/jqft documents) rather
    /// than exactly one document per source. Its absence declares the single-document fact.
    AdjacentValues,
    /// The edit lane: the format binds retained source spans and supplies the edit-render dialect and written splice
    /// policy, so `--edit` can splice the retained source instead of whole-re-encoding. The CLI's `--edit` gate reads
    /// THIS declaration rather than a hand-written format list; a codec that declares it without its receipts is the
    /// drift class the receipt lane exists to catch.
    Edit,
}

/// One complete conjunction of executable route guarantees.
#[derive(Debug)]
pub struct CapabilityBundle {
    footprint: AccessFootprintKind,
    result: AccessResultKind,
    demand: CodecDemand,
    guarantees: AccessGuarantees,
}

impl CapabilityBundle {
    /// Constructs one complete executable route conjunction.
    #[must_use]
    pub const fn new(
        footprint: AccessFootprintKind,
        result: AccessResultKind,
        demand: CodecDemand,
        guarantees: AccessGuarantees,
    ) -> Self {
        Self {
            footprint,
            result,
            demand,
            guarantees,
        }
    }
    /// Structural family implemented by the route.
    #[must_use]
    pub const fn footprint(&self) -> AccessFootprintKind {
        self.footprint
    }
    /// Result authority published by the route.
    #[must_use]
    pub const fn result(&self) -> AccessResultKind {
        self.result
    }
    /// Exact satisfiable information set.
    #[must_use]
    pub const fn demand(&self) -> &CodecDemand {
        &self.demand
    }
    /// Validation guarantee.
    #[must_use]
    pub const fn validation(&self) -> ValidationMode {
        self.guarantees.validation()
    }
    /// Diagnostic guarantee.
    #[must_use]
    pub const fn diagnostics(&self) -> DiagnosticPolicy {
        self.guarantees.diagnostics()
    }
}
