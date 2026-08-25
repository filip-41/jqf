//! Canonical codec information demands: clauses the binder matches.
//!
//! Clauses bind; delivering more than asked is sound. Sibling: [`crate::access`].

use alloc::vec::Vec;
use core::cmp::Ordering;

use jqf_data::{ExpandedName, FactKindId, FactRoleId};
use jqf_resource::{ResourceContext, ResourceError};

/// Exact topology information requested by engine analysis.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum TopologyDemand {
    /// Ordered child/member occurrences.
    Children,
    /// The occurrence owning a node.
    Owner,
}

/// Exact retained-source capability requested by engine analysis.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SourceCapabilityDemand {
    /// Exact byte ranges from the retained source.
    ByteRanges,
    /// The complete retained source snapshot.
    WholeSource,
}

/// One exact codec demand clause.
///
/// The vocabulary is closed, and production currently inserts exactly [`DemandClause::SemanticRoot`] and
/// [`DemandClause::ValueShape`]. Most other clauses name CONDITIONAL-DECODE mechanisms whose first producer does not
/// exist yet: fuzz tooling inserts them today, and codecs match them to decline. [`DemandClause::Attribute`] is the
/// partial exception: the binder can already bind an Attribute demand through the attribute-absence adapters whenever a
/// wired provider advertises that support, so its bind path works even before engine lowering produces the clause. A
/// new clause lands only together with its production inserter; an existing clause stays until the mechanism it names
/// ships (its doc names that mechanism).
#[derive(Debug, Eq, PartialEq)]
pub enum DemandClause {
    /// Semantic root projection.
    SemanticRoot,
    /// Exact payload-transparent semantic category.
    ValueShape,
    /// Authority to read the selected node's exact intrinsic tag fact. First producer: engine lowering for `.@tag`,
    /// letting a YAML-style decoder skip tag resolution when no program reads it.
    IntrinsicTag,
    /// Exact attached fact schema and role. The fine-grained upgrade of the requirement's coarse `FactIntent`: a
    /// `.@comment`-only program would demand exactly the comment fact instead of every attachment. First producer:
    /// engine lowering for named `.@fact` reads.
    AttachedFact {
        /// Exact fact schema.
        kind: FactKindId,
        /// Exact attachment role.
        role: FactRoleId,
    },
    /// Exact topology relation. First producer: markup `.@` selectors, so a codec records child/owner topology only
    /// when a selector needs it.
    Topology(TopologyDemand),
    /// Exact source capability. First producer: the canonical-identity lane and lazy span materializers declaring their
    /// retained-source re-reads explicitly instead of assuming them.
    Source(SourceCapabilityDemand),
    /// Exact expanded-name markup attribute. First producer: `.&name` lower- ing, letting XML/HTML skip building
    /// attribute maps nobody selects.
    Attribute(ExpandedName),
}

impl DemandClause {
    fn compare(&self, other: &Self) -> Ordering {
        let rank = |value: &Self| match value {
            Self::SemanticRoot => 0,
            Self::ValueShape => 1,
            Self::IntrinsicTag => 2,
            Self::AttachedFact { .. } => 3,
            Self::Topology(_) => 4,
            Self::Source(_) => 6,
            Self::Attribute(_) => 7,
        };
        rank(self).cmp(&rank(other)).then_with(|| match (self, other) {
            (Self::AttachedFact { kind: lk, role: lr }, Self::AttachedFact { kind: rk, role: rr }) => {
                lk.cmp(rk).then_with(|| lr.cmp(rr))
            }
            (Self::Topology(left), Self::Topology(right)) => left.cmp(right),
            (Self::Source(left), Self::Source(right)) => left.cmp(right),
            (Self::Attribute(left), Self::Attribute(right)) => left.cmp(right),
            _ => Ordering::Equal,
        })
    }
}

/// Canonical deterministic exact set of codec information clauses.
#[derive(Debug)]
pub struct CodecDemand {
    clauses: Vec<DemandClause>,
}

impl CodecDemand {
    /// Creates an empty exact set.
    pub fn try_new(_resources: &ResourceContext<'_>) -> Self {
        Self { clauses: Vec::new() }
    }

    /// Inserts one clause, preserving canonical order and exact deduplication.
    pub fn try_insert(&mut self, clause: &DemandClause) -> Result<(), ResourceError> {
        let clause = clone_clause(clause)?;
        match self.clauses.binary_search_by(|existing| existing.compare(&clause)) {
            Ok(_) => Ok(()),
            Err(at) => {
                // Keep the growth on the fallible path: the counting allocator can refuse an exact reservation, and the
                // allocation-failure-injection harness exercises that refusal through demand construction.
                self.clauses.try_reserve(1).map_err(jqf_resource::ResourceError::from)?;
                self.clauses.insert(at, clause);
                Ok(())
            }
        }
    }

    /// Returns canonical exact clauses.
    #[must_use]
    pub fn clauses(&self) -> &[DemandClause] {
        &self.clauses
    }

    /// Returns whether this set structurally contains every required clause.
    #[must_use]
    pub fn implies(&self, required: &Self) -> bool {
        required
            .clauses()
            .iter()
            .all(|needle| self.clauses().binary_search_by(|c| c.compare(needle)).is_ok())
    }

    /// Returns exact structural equivalence, independent of fingerprints.
    #[must_use]
    pub fn structurally_eq(&self, other: &Self) -> bool {
        self.clauses().len() == other.clauses().len()
            && self
                .clauses()
                .iter()
                .zip(other.clauses())
                .all(|(left, right)| left.compare(right) == Ordering::Equal)
    }

    /// Versioned deterministic observability/cache aid.
    #[must_use]
    pub fn fingerprint(&self) -> DemandFingerprint {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        mix(&mut hash, 1);
        for clause in self.clauses() {
            hash_clause(&mut hash, clause);
        }
        DemandFingerprint { value: hash }
    }

    /// The attribute-stripped copy an edit drive publishes over: attributes are presentation facts the re-encode
    /// re-derives from the source.
    pub(crate) fn try_clone_without_attributes(&self, resources: &ResourceContext<'_>) -> Result<Self, ResourceError> {
        let mut copy = Self::try_new(resources);
        for clause in self.clauses() {
            if matches!(clause, DemandClause::Attribute(_)) {
                continue;
            }
            copy.try_insert(clause)?;
        }
        Ok(copy)
    }
    /// Re-pairs every clause with freshly reserved storage so the copy's footprint is accounted against the consuming
    /// request.
    pub(crate) fn try_clone_in(&self, resources: &ResourceContext<'_>) -> Result<Self, ResourceError> {
        let mut copy = Self::try_new(resources);
        for clause in self.clauses() {
            copy.try_insert(clause)?;
        }
        Ok(copy)
    }
}

fn clone_clause(clause: &DemandClause) -> Result<DemandClause, ResourceError> {
    Ok(match clause {
        DemandClause::SemanticRoot => DemandClause::SemanticRoot,
        DemandClause::ValueShape => DemandClause::ValueShape,
        DemandClause::IntrinsicTag => DemandClause::IntrinsicTag,
        DemandClause::AttachedFact { kind, role } => DemandClause::AttachedFact {
            kind: kind.try_clone_accounted()?,
            role: role.try_clone_accounted()?,
        },
        DemandClause::Topology(value) => DemandClause::Topology(*value),
        DemandClause::Source(value) => DemandClause::Source(*value),
        DemandClause::Attribute(value) => DemandClause::Attribute(value.try_clone_accounted()?),
    })
}

/// Versioned non-authoritative demand fingerprint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DemandFingerprint {
    value: u64,
}

impl DemandFingerprint {
    /// Fingerprint bits.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.value
    }
}

fn mix(hash: &mut u64, byte: u8) {
    *hash ^= u64::from(byte);
    *hash = hash.wrapping_mul(0x100_0000_01b3);
}
fn mix_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        mix(hash, *byte);
    }
    mix(hash, 0xff);
}
fn hash_clause(hash: &mut u64, clause: &DemandClause) {
    match clause {
        DemandClause::SemanticRoot => mix(hash, 0),
        DemandClause::ValueShape => mix(hash, 1),
        DemandClause::IntrinsicTag => mix(hash, 2),
        DemandClause::AttachedFact { kind, role } => {
            // Keep the established fingerprint tag so removing an unimplemented clause does not renumber live clause
            // hashes.
            mix(hash, 4);
            mix_bytes(hash, kind.as_str().as_bytes());
            mix_bytes(hash, role.as_str().as_bytes());
        }
        DemandClause::Topology(value) => {
            mix(hash, 5);
            mix(hash, *value as u8);
        }
        DemandClause::Source(value) => {
            mix(hash, 7);
            mix(hash, *value as u8);
        }
        DemandClause::Attribute(value) => {
            mix(hash, 8);
            mix_bytes(hash, value.namespace_uri().as_bytes());
            mix_bytes(hash, value.local_name().as_bytes());
        }
    }
}
