//! Per-item semantic preservation evidence produced by encoders.
//!
//! An encoder that started under [`crate::PreservationRequest::Report`] retains one report. Sibling: [`crate::encode`].

/// Outcome for one independently assessed preservation axis.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreservationOutcome {
    /// The target representation retains the axis exactly.
    Exact,
    /// The target retains meaning after a canonical representation change.
    Normalized,
    /// The target deliberately leaves this axis out.
    Omitted,
    /// The codec cannot determine the outcome.
    Indeterminate,
}

/// Finalized preservation evidence for one encoded item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreservationReport {
    semantic_values: PreservationOutcome,
    tags_and_facts: PreservationOutcome,
    ordering: PreservationOutcome,
    presentation: PreservationOutcome,
}

impl PreservationReport {
    /// Creates a complete per-axis report.
    #[must_use]
    pub const fn new(
        semantic_values: PreservationOutcome,
        tags_and_facts: PreservationOutcome,
        ordering: PreservationOutcome,
        presentation: PreservationOutcome,
    ) -> Self {
        Self {
            semantic_values,
            tags_and_facts,
            ordering,
            presentation,
        }
    }

    /// Semantic-value preservation.
    #[must_use]
    pub const fn semantic_values(self) -> PreservationOutcome {
        self.semantic_values
    }
    /// Tag and fact preservation.
    #[must_use]
    pub const fn tags_and_facts(self) -> PreservationOutcome {
        self.tags_and_facts
    }
    /// Mapping and sequence ordering preservation.
    #[must_use]
    pub const fn ordering(self) -> PreservationOutcome {
        self.ordering
    }
    /// Presentation and source-reuse preservation.
    #[must_use]
    pub const fn presentation(self) -> PreservationOutcome {
        self.presentation
    }
}
