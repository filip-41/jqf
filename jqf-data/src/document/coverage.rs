//! What a published document actually kept.
//!
//! [`DocumentCoverage`] is the compact set readers check before asking for optional data. [`BuilderCoverage`] is what
//! the builder was asked to keep. [`AuthoritativeEmptyFamilies`] records families the decoder observed as absent.

use core::fmt;

/// One reader capability this document kept.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DocumentCapability {
    /// The canonical semantic root can be read.
    SemanticRoot,
    /// Every logical node has a retained semantic projection.
    SemanticNodes,
    /// Every node's intrinsic tag observation is retained.
    IntrinsicTags,
    /// Nodes and ordered topology occurrences can be read.
    Topology,
    /// Ordered attached facts can be read.
    AttachedFacts,
    /// The complete retained source snapshot can be read.
    WholeSource,
}

impl DocumentCapability {
    const fn bit(self) -> u16 {
        match self {
            Self::SemanticRoot => 1 << 0,
            Self::SemanticNodes => 1 << 1,
            Self::IntrinsicTags => 1 << 2,
            Self::Topology => 1 << 3,
            Self::AttachedFacts => 1 << 4,
            Self::WholeSource => 1 << 5,
        }
    }
}

/// A broad data observation family.
///
/// Families are deliberately separate from exact capabilities. In particular, a family asserted absent is authoritative
/// observation evidence — the publishing codec observed that the complete family cannot occur for this product; it is
/// not a statement about a target format's general features. The same law governs [`AuthoritativeEmptyFamilies`]' set
/// bits.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DocumentCapabilityFamily {
    /// Attached portable facts.
    AttachedFacts,
    /// Markup attributes identified by expanded names.
    Attributes,
    /// Observable source ranges and snapshots.
    Source,
}

impl DocumentCapabilityFamily {
    const fn bit(self) -> u16 {
        match self {
            Self::AttachedFacts => 1 << 0,
            Self::Attributes => 1 << 1,
            Self::Source => 1 << 2,
        }
    }
}

/// Zero-allocation authoritative absence declarations for broad data families.
///
/// A set bit means the document's publishing codec observed that the complete family cannot occur for this product —
/// the authoritative-absence law stated on [`DocumentCapabilityFamily`]. It does not manufacture an empty reader or
/// relax the requirement to retain data for families which are not declared here.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct AuthoritativeEmptyFamilies(u16);

impl AuthoritativeEmptyFamilies {
    /// No family is declared authoritatively absent.
    #[must_use]
    pub const fn none() -> Self {
        Self(0)
    }

    /// Creates a declaration for one broad family.
    #[must_use]
    pub const fn from_family(family: DocumentCapabilityFamily) -> Self {
        Self(family.bit())
    }

    /// Returns whether this declaration authoritatively covers `family`.
    #[must_use]
    pub const fn contains(self, family: DocumentCapabilityFamily) -> bool {
        (self.0 & family.bit()) != 0
    }
}

/// Retained successful-diagnostic authority.
///
/// Nonempty diagnostic records and handles are not yet retained; these two header-only states are allocation-free and
/// distinguish no request from a complete observed empty result.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum DiagnosticCoverage {
    /// Diagnostics were not requested for this product.
    #[default]
    NotRequested,
    /// Diagnostics were requested and the publisher authoritatively observed that there are none.
    AuthoritativeEmpty,
}

/// Compact immutable retained-data authority for one document.
///
/// The coverage lives with the authoritative storage rather than an access report — see the module doc for the
/// rejection law: a reader refuses unretained optional information instead of fabricating it. Complete builders publish
/// all present reader capabilities; a sparse builder may publish a strict subset only when the corresponding reader
/// data is intentionally omitted.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DocumentCoverage {
    capabilities: u16,
    empty_families: AuthoritativeEmptyFamilies,
    diagnostics: DiagnosticCoverage,
}

impl DocumentCoverage {
    /// Complete coverage for a document without a retained source snapshot.
    #[must_use]
    pub const fn complete_without_source() -> Self {
        Self {
            capabilities: DocumentCapability::SemanticRoot.bit()
                | DocumentCapability::SemanticNodes.bit()
                | DocumentCapability::IntrinsicTags.bit()
                | DocumentCapability::Topology.bit()
                | DocumentCapability::AttachedFacts.bit(),
            empty_families: AuthoritativeEmptyFamilies::none(),
            diagnostics: DiagnosticCoverage::NotRequested,
        }
    }

    /// Zero-capability coverage for building a test fixture.
    #[cfg(test)]
    const fn empty() -> Self {
        Self {
            capabilities: 0,
            empty_families: AuthoritativeEmptyFamilies::none(),
            diagnostics: DiagnosticCoverage::NotRequested,
        }
    }

    /// Adds one exact capability to a test fixture.
    #[cfg(test)]
    const fn with_capability(mut self, capability: DocumentCapability) -> Self {
        self.capabilities |= capability.bit();
        self
    }

    /// Sets the publisher's authoritative-absence declarations.
    pub(crate) const fn with_authoritative_empty_families(mut self, families: AuthoritativeEmptyFamilies) -> Self {
        self.empty_families = families;
        self
    }

    /// Sets the successful-diagnostic authority.
    pub(crate) const fn with_diagnostic_coverage(mut self, diagnostics: DiagnosticCoverage) -> Self {
        self.diagnostics = diagnostics;
        self
    }

    /// Returns whether exact retained-data capability is available.
    #[must_use]
    pub const fn contains(self, capability: DocumentCapability) -> bool {
        (self.capabilities & capability.bit()) != 0
    }

    /// Returns the publisher's authoritative-empty family declarations.
    #[must_use]
    pub const fn authoritative_empty_families(self) -> AuthoritativeEmptyFamilies {
        self.empty_families
    }

    /// Returns successful-diagnostic authority.
    #[must_use]
    pub const fn diagnostic_coverage(self) -> DiagnosticCoverage {
        self.diagnostics
    }
}

impl Default for DocumentCoverage {
    fn default() -> Self {
        Self::complete_without_source()
    }
}

/// Demanded side-data coverage a builder physically constructs.
///
/// Mandatory semantics — the semantic root, every semantic node, intrinsic tags, and non-core `Value::Tagged`
/// wrappers — are always built and are not represented here. Each flag gates one optional side-data family: when a
/// family is not demanded, its arenas are neither reserved nor populated, and builder writes that would populate it are
/// rejected with a typed error rather than silently dropped. Whole-source coverage is likewise not represented here
/// because no construction path can produce it: the builder constructors reject a non-`None` source and the finalizer's
/// borrowed-source attachment does not widen builder coverage either.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BuilderCoverage {
    topology: bool,
    attached_facts: bool,
}

impl BuilderCoverage {
    /// Builds every optional side-data family (the default).
    #[must_use]
    pub const fn complete() -> Self {
        Self {
            topology: true,
            attached_facts: true,
        }
    }

    /// Builds only mandatory semantics: no topology or facts.
    #[must_use]
    pub const fn minimal_semantic() -> Self {
        Self {
            topology: false,
            attached_facts: false,
        }
    }

    /// Returns a copy retaining rich occurrence topology when `retained`.
    #[must_use]
    pub const fn with_topology(mut self, retained: bool) -> Self {
        self.topology = retained;
        self
    }

    /// Returns a copy retaining attached facts when `retained`.
    #[must_use]
    pub const fn with_attached_facts(mut self, retained: bool) -> Self {
        self.attached_facts = retained;
        self
    }

    /// Returns whether rich occurrence topology is demanded.
    #[must_use]
    pub const fn topology(self) -> bool {
        self.topology
    }

    /// Returns whether attached facts are demanded.
    #[must_use]
    pub const fn attached_facts(self) -> bool {
        self.attached_facts
    }

    /// Derives the published coverage this demand yields for one product.
    ///
    /// Mandatory semantics are always present; source attachment is a post-publication operation and does not widen
    /// builder coverage.
    pub(crate) const fn to_document_coverage(self) -> DocumentCoverage {
        let mut capabilities = DocumentCapability::SemanticRoot.bit()
            | DocumentCapability::SemanticNodes.bit()
            | DocumentCapability::IntrinsicTags.bit();
        if self.topology {
            capabilities |= DocumentCapability::Topology.bit();
        }
        if self.attached_facts {
            capabilities |= DocumentCapability::AttachedFacts.bit();
        }
        DocumentCoverage {
            capabilities,
            empty_families: AuthoritativeEmptyFamilies::none(),
            diagnostics: DiagnosticCoverage::NotRequested,
        }
    }
}

impl Default for BuilderCoverage {
    fn default() -> Self {
        Self::complete()
    }
}

impl fmt::Display for DocumentCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SemanticRoot => "semantic root",
            Self::SemanticNodes => "all semantic nodes",
            Self::IntrinsicTags => "intrinsic tags",
            Self::Topology => "complete topology",
            Self::AttachedFacts => "attached facts",
            Self::WholeSource => "whole source",
        })
    }
}

impl fmt::Display for DocumentCapabilityFamily {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AttachedFacts => "attached facts",
            Self::Attributes => "attributes",
            Self::Source => "source",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{DocumentCapability, DocumentCoverage};

    #[test]
    fn complete_coverage_retains_intrinsic_tags_without_whole_source() {
        let coverage = DocumentCoverage::complete_without_source();
        assert!(coverage.contains(DocumentCapability::IntrinsicTags));
        assert!(!coverage.contains(DocumentCapability::WholeSource));
    }

    #[test]
    fn intrinsic_authority_is_independent_from_topology() {
        let coverage = DocumentCoverage::empty()
            .with_capability(DocumentCapability::SemanticRoot)
            .with_capability(DocumentCapability::SemanticNodes)
            .with_capability(DocumentCapability::IntrinsicTags);
        assert!(coverage.contains(DocumentCapability::IntrinsicTags));
        assert!(!coverage.contains(DocumentCapability::Topology));
    }
}
