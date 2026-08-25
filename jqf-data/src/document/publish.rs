//! Finish a builder in cooperative polls.
//!
//! [`AccountedDocumentFinalizer`] does a bounded slice of work per poll and returns
//! [`DocumentFinalizationPoll::Pending`] until the document is ready. [`poll`](AccountedDocumentFinalizer::poll) drops
//! any source a prior [`poll_with_source`](AccountedDocumentFinalizer::poll_with_source) installed.

use super::builder::AccountedDocumentBuilder;
use super::storage::StorageRange;
use super::{
    AccountedLocalDateTime, AccountedLocalTime, AccountedOffsetDateTime, AuthoritativeEmptyFamilies, BuilderCoverage,
    DataError, DiagnosticCoverage, Document, DocumentCapabilityFamily, DocumentCoverage, DocumentStorage,
    DocumentStorageOwner, FactId, LocalOwnerRef, NodeId, NodeRecord, NodeSemantic, OccurrenceId, OccurrenceRecord,
    StoredDocumentFact, WidePayload,
};
use alloc::vec::Vec;
use core::marker::PhantomData;
use jqf_resource::ResourceContext;

fn validate_coverage(
    coverage: DocumentCoverage,
    evidence: &CoverageEvidence<'_>,
) -> Result<DocumentCoverage, DataError> {
    if coverage.contains(super::DocumentCapability::WholeSource) {
        return Err(DataError::InvalidDocument);
    }
    if !coverage.contains(super::DocumentCapability::IntrinsicTags)
        && evidence.nodes.iter().any(|node| node.intrinsic_tag.is_present())
    {
        return Err(DataError::InvalidDocument);
    }
    Ok(coverage)
}

/// The retained content a coverage claim is validated against: the node, occurrence, and fact tables.
pub(super) struct CoverageEvidence<'storage> {
    pub(super) nodes: &'storage [NodeRecord],
    /// The side arena the wide node semantics point into. A node header alone cannot answer whether the node names
    /// source bytes: a wide decimal's coefficient reference lives here.
    pub(super) wide: &'storage [WidePayload],
    pub(super) occurrences: &'storage [OccurrenceRecord],
    pub(super) facts: &'storage [StoredDocumentFact],
    /// Out-of-band authored spans for nodes and facts. A document names source bytes without any node semantic carrying
    /// a [`TextRef::Source`](super::TextRef::Source) when its value text was built in the arena and only the token span
    /// was authored against the seal, so empty-Source validation must see these too.
    pub(super) authored_spans: &'storage [super::storage::AuthoredSpanRecord],
}

fn validate_empty_families(
    families: AuthoritativeEmptyFamilies,
    evidence: &CoverageEvidence<'_>,
) -> Result<(), DataError> {
    let contradiction = if families.contains(DocumentCapabilityFamily::AttachedFacts) && !evidence.facts.is_empty() {
        Some(DocumentCapabilityFamily::AttachedFacts)
    } else if families.contains(DocumentCapabilityFamily::Source)
        && (evidence.nodes.iter().any(|node| {
            // Strings are not the only semantic that may name source bytes: an integer whose canonical spelling is
            // verbatim in the input is stored as a source span too, and it makes the Source family just as non-empty.
            matches!(
                node.semantic,
                NodeSemantic::Text(super::TextRef::Source(_))
                        | NodeSemantic::StoredInteger(super::TextRef::Source(_))
                        // A deferred container names source bytes exactly as a zero-copy string does, so it makes the
                        // Source family non-empty for the same reason.
                        | NodeSemantic::ContainerSpan {
                            text: super::TextRef::Source(_),
                            ..
                        }
            )
        }) || evidence.wide.iter().any(|payload| {
            // A wide decimal's coefficient is a text reference like any other, so a source-backed one makes the family
            // non-empty for the same reason the node-header semantics do.
            matches!(
                payload,
                WidePayload::StoredDecimal {
                    coefficient: super::TextRef::Source(_),
                    ..
                }
            )
        }) || evidence
            .occurrences
            .iter()
            .any(|occurrence| matches!(occurrence.key, Some(super::TextRef::Source(_))))
            // An authored span or a fact-level source span names observable source bytes exactly as a source-backed
            // semantic does, so it makes the family non-empty for the same reason.
            || !evidence.authored_spans.is_empty()
            || evidence.facts.iter().any(|fact| fact.source_span().is_some()))
    {
        Some(DocumentCapabilityFamily::Source)
    } else {
        None
    };
    contradiction.map_or(Ok(()), |family| Err(DataError::ContradictoryCoverage { family }))
}

/// Resolves demanded, empty-families, and diagnostic coverage into one published [`DocumentCoverage`] and validates it
/// against the evidence; an empty-family declaration that contradicts retained content raises `ContradictoryCoverage`.
pub(super) fn published_coverage(
    demanded: BuilderCoverage,
    empty_families: AuthoritativeEmptyFamilies,
    diagnostics: DiagnosticCoverage,
    evidence: &CoverageEvidence<'_>,
) -> Result<DocumentCoverage, DataError> {
    validate_empty_families(empty_families, evidence)?;
    let coverage = demanded
        .to_document_coverage()
        .with_authoritative_empty_families(empty_families)
        .with_diagnostic_coverage(diagnostics);
    validate_coverage(coverage, evidence)
}

/// Result of one finalization poll.
pub enum DocumentFinalizationPoll<'source> {
    /// Need another poll. State is kept.
    Pending,
    /// The document is ready. Happens once.
    Ready(Document<'source>),
}

/// The cooperative publication phase machine: the resumable single-pass adjacency derivation, the counting-sort
/// fallback for authoring orders the stack pass cannot classify, the fact-owner index, then publish.
pub(super) enum FinalizationPhase {
    Reserve {
        credited: usize,
    },
    DeriveAdjacency,
    /// Zeros `counts` one bounded credit at a time: for the counting-sort when `for_facts` is false, for the fact-owner
    /// index after the single-pass adjacency when true.
    InitCounts {
        node: usize,
        for_facts: bool,
    },
    CountOccurrences {
        occurrence: usize,
    },
    AssignRanges {
        node: usize,
        total: usize,
    },
    InitOwnerOccurrences {
        cursor: usize,
        total: usize,
    },
    FillOwnerOccurrences {
        occurrence: usize,
    },
    IndexFactOwners {
        step: FactIndexStep,
    },
    Publish,
    Complete,
}

/// One step of the fact-owner index build: zero the counts, count facts per node, assign ranges, then fill the grouped
/// ids.
#[derive(Clone, Copy)]
pub(super) enum FactIndexStep {
    Zero { node: usize },
    Count { fact: usize },
    Assign { node: usize, start: usize },
    Fill { fact: usize },
}

/// Owning continuation for cooperative accounted-document publication.
pub struct AccountedDocumentFinalizer<'source> {
    pub(super) builder: Option<AccountedDocumentBuilder<'source>>,
    pub(super) root: NodeId,
    pub(super) phase: FinalizationPhase,
    /// The in-progress resumable adjacency pass; replaced by `owner_occurrences` when it completes, or dropped for the
    /// counting-sort fallback.
    pub(super) adjacency: Option<super::adjacency::CooperativeAdjacency>,
    pub(super) counts: Vec<usize>,
    pub(super) owner_occurrences: Vec<OccurrenceId>,
    /// The in-progress fact-owner index, published into the document when complete. Empty (and skipped) when the
    /// document has no facts.
    pub(super) fact_owner_nodes: Vec<u32>,
    pub(super) fact_owner_ranges: Vec<StorageRange>,
    pub(super) fact_owner_ids: Vec<FactId>,
    /// Scratch parked for the NEXT document: the builder's authoring vectors land here as publication releases them, so
    /// they outlive the builder and leave together at [`Self::reclaim_transients`].
    pub(super) spare: super::DocumentTransients,
}

struct FinalizationSourceGuard<'source> {
    builder: *mut Option<AccountedDocumentBuilder<'source>>,
}

impl Drop for FinalizationSourceGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: the pointer names the finalizer's `builder` field for the
        // duration of the guarded poll. Publication may take that option, in
        // which case there is no source slot left to clear.
        unsafe {
            if let Some(builder) = (*self.builder).as_mut() {
                builder.finalization_source = None;
            }
        }
    }
}

impl<'source> AccountedDocumentFinalizer<'source> {
    /// Installs recycled finalization scratch from a previous document.
    ///
    /// CAPACITY ONLY, and only valid immediately after construction, while every vector here is still the empty one
    /// `begin_finish` created.
    pub fn install_transients(&mut self, spare: &mut super::DocumentTransients) {
        core::mem::swap(&mut self.counts, &mut spare.counts);
        core::mem::swap(&mut self.owner_occurrences, &mut spare.owner_occurrences);
        core::mem::swap(&mut self.fact_owner_nodes, &mut spare.fact_owner_nodes);
        core::mem::swap(&mut self.fact_owner_ranges, &mut spare.fact_owner_ranges);
        core::mem::swap(&mut self.fact_owner_ids, &mut spare.fact_owner_ids);
    }

    /// Hands every recycled vector back to the caller's bag once the document is published.
    ///
    /// Collects both this finalizer's own scratch and the builder scratch publication parked along the way. Each vector
    /// is cleared, and one that outgrew the retention cap is released rather than carried.
    pub fn reclaim_transients(&mut self, spare: &mut super::DocumentTransients) {
        super::transients::park(&mut self.spare.counts, &mut self.counts);
        super::transients::park(&mut self.spare.owner_occurrences, &mut self.owner_occurrences);
        super::transients::park(&mut self.spare.fact_owner_nodes, &mut self.fact_owner_nodes);
        super::transients::park(&mut self.spare.fact_owner_ranges, &mut self.fact_owner_ranges);
        super::transients::park(&mut self.spare.fact_owner_ids, &mut self.fact_owner_ids);
        core::mem::swap(&mut self.spare, spare);
    }

    /// Performs bounded finalization work and retains all continuation state on `Pending`. A plain poll drops any
    /// trusted-source attachment — see the module doc.
    pub fn poll(
        &mut self,
        resources: &mut ResourceContext<'_>,
    ) -> Result<DocumentFinalizationPoll<'source>, DataError> {
        if let Some(builder) = self.builder.as_mut() {
            builder.finalization_source = None;
        }
        self.poll_internal(resources)
    }

    /// Performs bounded finalization with temporary access to the exact sealed immutable source authority.
    ///
    /// # Safety
    ///
    /// `source` must be the same immutable source segment used by the binding stage and must remain immutable for this
    /// complete call. The caller must enforce that continuity across every poll; metadata equality is not proof.
    ///
    /// A metadata-mismatched source is a misuse: the finalizer is permanently poisoned and this and every later call
    /// answer `ReaderFailed` (never `InvalidDocument`), so the caller treats that outcome as terminal.
    #[doc(hidden)]
    pub unsafe fn poll_with_source(
        &mut self,
        source: jqf_source::ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<DocumentFinalizationPoll<'source>, DataError> {
        let builder_slot = &raw mut self.builder;
        {
            let builder = self.builder.as_mut().ok_or(DataError::ReaderFailed)?;
            let binding = builder.source_binding.ok_or(DataError::InvalidDocument)?;
            if !binding.seal().metadata_matches(source) {
                self.phase = FinalizationPhase::Complete;
                return Err(DataError::ReaderFailed);
            }
            builder.finalization_source = Some((source.bytes().as_ptr(), source.bytes().len()));
        }
        // The guard clears the call-local source view on Pending, error, and Ready, so a subsequent poll can never
        // observe a stale pointer.
        let _source_guard = FinalizationSourceGuard { builder: builder_slot };
        self.poll_internal(resources)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one finalization phase machine; splitting the arms would hide the phase order it enforces"
    )]
    fn poll_internal(
        &mut self,
        resources: &mut ResourceContext<'_>,
    ) -> Result<DocumentFinalizationPoll<'source>, DataError> {
        // `owner_positions` is per-node authoring scratch: `add_occurrence` reads and bumps each owner's role-position
        // counter to stamp the occurrence's stored `position`, and nothing past authoring reads it — neither the
        // counting-sort finalizer nor the relationship-arena emit touches it. On a large document it is the single
        // largest transient (4 bytes per (role, node) cell; two roles ≈ 8 bytes per node), so release it here, at the
        // first finalization poll, before the finalizer allocates its counts, owner-occurrence, and relationship arenas
        // on top of it, instead of holding it live under the finalization peak until the builder drops. The guard makes
        // it a one-shot across cooperative re-entry of THIS finalizer: after the swap the builder holds an empty slot,
        // so later polls stay quiet. A recycled NEXT document installs a retained table and parks it once again —
        // once per document is the law, never twice for one build.
        if let Some(builder) = self.builder.as_mut()
            && builder.owner_positions.capacity() > 0
        {
            // Parking instead of dropping keeps the release exactly as early as before — the table is off the builder
            // before the finalizer allocates on top of it — while a record-shaped table survives for the next
            // document. A table past the retention cap is still dropped outright, so the large-document peak is
            // unchanged.
            self.spare.park_owner_positions(&mut builder.owner_positions);
        }
        // Publish every staged builder record before the phase machine reads the arenas (the reserve counts, adjacency
        // passes, and arena emit all walk the physical lengths). A partial staging block always remains after
        // authoring, so this flush is the cooperative twin of the eager `finish` flush.
        if let Some(builder) = self.builder.as_mut() {
            builder.flush_staged()?;
        }
        loop {
            let phase = core::mem::replace(&mut self.phase, FinalizationPhase::Complete);
            self.phase = match phase {
                FinalizationPhase::Reserve { mut credited } => {
                    let builder = self.builder.as_ref().ok_or(DataError::InvalidDocument)?;
                    let total = builder
                        .nodes
                        .len()
                        .checked_add(builder.occurrences.len())
                        .ok_or(DataError::ArithmeticOverflow)?;
                    match resources.admit_work_bytes(total - credited)? {
                        jqf_resource::WorkAdmission::Pending => {
                            self.phase = FinalizationPhase::Reserve { credited };
                            return Ok(DocumentFinalizationPoll::Pending);
                        }
                        jqf_resource::WorkAdmission::Granted(granted) => credited += granted,
                    }
                    if credited == total {
                        // The counting-sort fallback reserves `counts` on its own entry (fast-path documents never
                        // allocate it); see the `DeriveAdjacency` arm.
                        FinalizationPhase::DeriveAdjacency
                    } else {
                        FinalizationPhase::Reserve { credited }
                    }
                }
                FinalizationPhase::DeriveAdjacency => {
                    let builder = self.builder.as_mut().ok_or(DataError::InvalidDocument)?;
                    if self.adjacency.is_none() {
                        if super::adjacency::open_marker_reachable(builder.occurrences.len()) {
                            // The finished-range start could collide with the open marker; the stack pass cannot apply
                            // and the order-agnostic counting-sort takes over, exactly as in the synchronous `finish`
                            // path.
                            self.counts
                                .try_reserve_exact(builder.nodes.len())
                                .map_err(jqf_resource::ResourceError::from)?;
                            FinalizationPhase::InitCounts {
                                node: 0,
                                for_facts: false,
                            }
                        } else {
                            self.adjacency =
                                Some(super::adjacency::CooperativeAdjacency::new(builder.occurrences.len())?);
                            FinalizationPhase::DeriveAdjacency
                        }
                    } else {
                        let remaining = self.adjacency.as_ref().ok_or(DataError::InvalidDocument)?.remaining();
                        let jqf_resource::WorkAdmission::Granted(granted) =
                            resources.admit_work_transitions(remaining)?
                        else {
                            self.phase = FinalizationPhase::DeriveAdjacency;
                            return Ok(DocumentFinalizationPoll::Pending);
                        };
                        let adjacency = self.adjacency.as_mut().ok_or(DataError::InvalidDocument)?;
                        let finished =
                            adjacency.step(granted, builder.nodes.as_mut_slice(), builder.occurrences.as_slice())?;
                        if finished {
                            let adjacency = self.adjacency.take().ok_or(DataError::InvalidDocument)?;
                            if adjacency.is_fallback() {
                                // Non-stack-disciplined authoring: the counting-sort reassigns every node's range
                                // (including any open-frame markers the pass wrote), so no marker cleanup is owed here.
                                self.counts
                                    .try_reserve_exact(builder.nodes.len())
                                    .map_err(jqf_resource::ResourceError::from)?;
                                FinalizationPhase::InitCounts {
                                    node: 0,
                                    for_facts: false,
                                }
                            } else {
                                self.owner_occurrences = adjacency
                                    .finish(builder.nodes.as_mut_slice())?
                                    .ok_or(DataError::InvalidDocument)?;
                                if builder.facts.is_empty() {
                                    FinalizationPhase::Publish
                                } else {
                                    // The fact-owner index reuses `counts` as its per-node scatter scratch; the fast
                                    // path never allocated it, so reserve it here and zero-fill through `InitCounts`
                                    // before the index phases. `InitCounts` already leaves the array all-zero, so the
                                    // fact-index `Zero` step is skipped at the for_facts completion edge.
                                    self.counts
                                        .try_reserve_exact(builder.nodes.len())
                                        .map_err(jqf_resource::ResourceError::from)?;
                                    FinalizationPhase::InitCounts {
                                        node: 0,
                                        for_facts: true,
                                    }
                                }
                            }
                        } else {
                            FinalizationPhase::DeriveAdjacency
                        }
                    }
                }
                FinalizationPhase::InitCounts { node, for_facts } => {
                    let builder = self.builder.as_ref().ok_or(DataError::InvalidDocument)?;
                    if node == builder.nodes.len() {
                        if for_facts {
                            // `InitCounts` just zero-filled `counts`; skip the fact-index Zero pass (the counting-sort
                            // entry still needs it — it overwrote counts as scatter offsets).
                            FinalizationPhase::IndexFactOwners {
                                step: FactIndexStep::Count { fact: 0 },
                            }
                        } else {
                            FinalizationPhase::CountOccurrences { occurrence: 0 }
                        }
                    } else {
                        let remaining = builder.nodes.len() - node;
                        let jqf_resource::WorkAdmission::Granted(granted) =
                            resources.admit_work_transitions(remaining)?
                        else {
                            self.phase = FinalizationPhase::InitCounts { node, for_facts };
                            return Ok(DocumentFinalizationPoll::Pending);
                        };
                        self.counts.resize(self.counts.len() + granted, 0);
                        FinalizationPhase::InitCounts {
                            node: node + granted,
                            for_facts,
                        }
                    }
                }
                FinalizationPhase::CountOccurrences { occurrence } => {
                    let builder = self.builder.as_ref().ok_or(DataError::InvalidDocument)?;
                    if occurrence == builder.occurrences.len() {
                        FinalizationPhase::AssignRanges { node: 0, total: 0 }
                    } else {
                        let remaining = builder.occurrences.len() - occurrence;
                        let jqf_resource::WorkAdmission::Granted(granted) =
                            resources.admit_work_transitions(remaining)?
                        else {
                            self.phase = FinalizationPhase::CountOccurrences { occurrence };
                            return Ok(DocumentFinalizationPoll::Pending);
                        };
                        for cursor in occurrence..occurrence + granted {
                            if let LocalOwnerRef::Node(node) = builder.occurrences.as_slice()[cursor].owner {
                                let index = node.index();
                                self.counts.as_mut_slice()[index] = self.counts.as_slice()[index]
                                    .checked_add(1)
                                    .ok_or(DataError::ArithmeticOverflow)?;
                            }
                        }
                        FinalizationPhase::CountOccurrences {
                            occurrence: occurrence + granted,
                        }
                    }
                }
                FinalizationPhase::AssignRanges { node, total } => {
                    let builder = self.builder.as_mut().ok_or(DataError::InvalidDocument)?;
                    if node == builder.nodes.len() {
                        // `total` is now the exact node-owned occurrence count (every node's finished range accumulated
                        // into it); reserve `owner_occurrences` at exactly that length instead of the amortized growth
                        // `InitOwnerOccurrences` would otherwise drive it through one push at a time.
                        self.owner_occurrences
                            .try_reserve_exact(total)
                            .map_err(jqf_resource::ResourceError::from)?;
                        FinalizationPhase::InitOwnerOccurrences { cursor: 0, total }
                    } else {
                        let remaining = builder.nodes.len() - node;
                        let jqf_resource::WorkAdmission::Granted(granted) =
                            resources.admit_work_transitions(remaining)?
                        else {
                            self.phase = FinalizationPhase::AssignRanges { node, total };
                            return Ok(DocumentFinalizationPoll::Pending);
                        };
                        let mut next_total = total;
                        for cursor in node..node + granted {
                            let count = self.counts.as_mut_slice()[cursor];
                            // The count is consumed into `occurrence_range` here, so write the zero
                            // `FillOwnerOccurrences` needs as a per-node running offset back in the same pass — one
                            // pass over the nodes instead of a second zeroing walk before the fill.
                            self.counts.as_mut_slice()[cursor] = 0;
                            builder.nodes.as_mut_slice()[cursor].occurrence_range =
                                StorageRange::try_new(next_total, count)?;
                            next_total = next_total.checked_add(count).ok_or(DataError::ArithmeticOverflow)?;
                        }
                        FinalizationPhase::AssignRanges {
                            node: node + granted,
                            total: next_total,
                        }
                    }
                }
                FinalizationPhase::InitOwnerOccurrences { cursor, total } => {
                    if cursor == total {
                        FinalizationPhase::FillOwnerOccurrences { occurrence: 0 }
                    } else {
                        let remaining = total - cursor;
                        let jqf_resource::WorkAdmission::Granted(granted) =
                            resources.admit_work_transitions(remaining)?
                        else {
                            self.phase = FinalizationPhase::InitOwnerOccurrences { cursor, total };
                            return Ok(DocumentFinalizationPoll::Pending);
                        };
                        let placeholder = OccurrenceId::try_from_index(0).ok_or(DataError::ArithmeticOverflow)?;
                        self.owner_occurrences
                            .resize(self.owner_occurrences.len() + granted, placeholder);
                        FinalizationPhase::InitOwnerOccurrences {
                            cursor: cursor + granted,
                            total,
                        }
                    }
                }
                FinalizationPhase::FillOwnerOccurrences { occurrence } => {
                    let builder = self.builder.as_ref().ok_or(DataError::InvalidDocument)?;
                    if occurrence == builder.occurrences.len() {
                        if builder.facts.is_empty() {
                            FinalizationPhase::Publish
                        } else {
                            FinalizationPhase::IndexFactOwners {
                                step: FactIndexStep::Zero { node: 0 },
                            }
                        }
                    } else {
                        let remaining = builder.occurrences.len() - occurrence;
                        let jqf_resource::WorkAdmission::Granted(granted) =
                            resources.admit_work_transitions(remaining)?
                        else {
                            self.phase = FinalizationPhase::FillOwnerOccurrences { occurrence };
                            return Ok(DocumentFinalizationPoll::Pending);
                        };
                        for cursor in occurrence..occurrence + granted {
                            if let LocalOwnerRef::Node(node) = builder.occurrences.as_slice()[cursor].owner {
                                let node_index = node.index();
                                let range = builder.nodes.as_slice()[node_index].occurrence_range;
                                let offset = self.counts.as_slice()[node_index];
                                let destination = (range.start as usize)
                                    .checked_add(offset)
                                    .ok_or(DataError::ArithmeticOverflow)?;
                                self.owner_occurrences.as_mut_slice()[destination] =
                                    OccurrenceId::try_from_index(cursor).ok_or(DataError::ArithmeticOverflow)?;
                                self.counts.as_mut_slice()[node_index] =
                                    offset.checked_add(1).ok_or(DataError::ArithmeticOverflow)?;
                            }
                        }
                        FinalizationPhase::FillOwnerOccurrences {
                            occurrence: occurrence + granted,
                        }
                    }
                }
                FinalizationPhase::IndexFactOwners { step } => {
                    match step {
                        FactIndexStep::Zero { node } => {
                            let builder = self.builder.as_ref().ok_or(DataError::InvalidDocument)?;
                            if node == builder.nodes.len() {
                                FinalizationPhase::IndexFactOwners {
                                    step: FactIndexStep::Count { fact: 0 },
                                }
                            } else {
                                let remaining = builder.nodes.len() - node;
                                let jqf_resource::WorkAdmission::Granted(granted) =
                                    resources.admit_work_transitions(remaining)?
                                else {
                                    self.phase = FinalizationPhase::IndexFactOwners {
                                        step: FactIndexStep::Zero { node },
                                    };
                                    return Ok(DocumentFinalizationPoll::Pending);
                                };
                                self.counts.as_mut_slice()[node..node + granted].fill(0);
                                FinalizationPhase::IndexFactOwners {
                                    step: FactIndexStep::Zero { node: node + granted },
                                }
                            }
                        }
                        FactIndexStep::Count { fact } => {
                            let builder = self.builder.as_ref().ok_or(DataError::InvalidDocument)?;
                            if fact == builder.facts.len() {
                                // Each nonzero count creates at most one owner row, and a distinct owner owns at least
                                // one fact, so distinct owners cannot exceed the fact count either: the smaller bound
                                // is the exact reservation, matching the one-shot `FactOwnerIndex::build`.
                                let owner_upper_bound = builder.facts.len().min(self.counts.len());
                                self.fact_owner_nodes
                                    .try_reserve_exact(owner_upper_bound)
                                    .map_err(jqf_resource::ResourceError::from)?;
                                self.fact_owner_ranges
                                    .try_reserve_exact(owner_upper_bound)
                                    .map_err(jqf_resource::ResourceError::from)?;
                                FinalizationPhase::IndexFactOwners {
                                    step: FactIndexStep::Assign { node: 0, start: 0 },
                                }
                            } else {
                                let remaining = builder.facts.len() - fact;
                                let jqf_resource::WorkAdmission::Granted(granted) =
                                    resources.admit_work_transitions(remaining)?
                                else {
                                    self.phase = FinalizationPhase::IndexFactOwners {
                                        step: FactIndexStep::Count { fact },
                                    };
                                    return Ok(DocumentFinalizationPoll::Pending);
                                };
                                for cursor in fact..fact + granted {
                                    if let LocalOwnerRef::Node(node) = builder.facts.as_slice()[cursor].owner {
                                        let index = node.index();
                                        self.counts.as_mut_slice()[index] = self.counts.as_slice()[index]
                                            .checked_add(1)
                                            .ok_or(DataError::ArithmeticOverflow)?;
                                    }
                                }
                                FinalizationPhase::IndexFactOwners {
                                    step: FactIndexStep::Count { fact: fact + granted },
                                }
                            }
                        }
                        FactIndexStep::Assign { node, start } => {
                            let builder = self.builder.as_ref().ok_or(DataError::InvalidDocument)?;
                            if node == builder.nodes.len() {
                                self.fact_owner_ids
                                    .try_reserve_exact(start)
                                    .map_err(jqf_resource::ResourceError::from)?;
                                let placeholder = FactId::try_from_index(0).ok_or(DataError::ArithmeticOverflow)?;
                                self.fact_owner_ids.resize(start, placeholder);
                                FinalizationPhase::IndexFactOwners {
                                    step: FactIndexStep::Fill { fact: 0 },
                                }
                            } else {
                                let remaining = builder.nodes.len() - node;
                                let jqf_resource::WorkAdmission::Granted(granted) =
                                    resources.admit_work_transitions(remaining)?
                                else {
                                    self.phase = FinalizationPhase::IndexFactOwners {
                                        step: FactIndexStep::Assign { node, start },
                                    };
                                    return Ok(DocumentFinalizationPoll::Pending);
                                };
                                let mut next_start = start;
                                for cursor in node..node + granted {
                                    let count = self.counts.as_slice()[cursor];
                                    if count != 0 {
                                        self.fact_owner_nodes
                                            .push(u32::try_from(cursor).map_err(|_| DataError::ArithmeticOverflow)?);
                                        self.fact_owner_ranges.push(StorageRange::try_new(next_start, count)?);
                                        // Overwrite the count with the range's START: the Fill step consumes it as the
                                        // absolute write position and increments it per fact, so no separate offset
                                        // array is needed.
                                        self.counts.as_mut_slice()[cursor] = next_start;
                                        next_start =
                                            next_start.checked_add(count).ok_or(DataError::ArithmeticOverflow)?;
                                    }
                                }
                                FinalizationPhase::IndexFactOwners {
                                    step: FactIndexStep::Assign {
                                        node: node + granted,
                                        start: next_start,
                                    },
                                }
                            }
                        }
                        FactIndexStep::Fill { fact } => {
                            let builder = self.builder.as_ref().ok_or(DataError::InvalidDocument)?;
                            if fact == builder.facts.len() {
                                FinalizationPhase::Publish
                            } else {
                                let remaining = builder.facts.len() - fact;
                                let jqf_resource::WorkAdmission::Granted(granted) =
                                    resources.admit_work_transitions(remaining)?
                                else {
                                    self.phase = FinalizationPhase::IndexFactOwners {
                                        step: FactIndexStep::Fill { fact },
                                    };
                                    return Ok(DocumentFinalizationPoll::Pending);
                                };
                                for cursor in fact..fact + granted {
                                    if let LocalOwnerRef::Node(node) = builder.facts.as_slice()[cursor].owner {
                                        let index = node.index();
                                        let position = self.counts.as_slice()[index];
                                        self.fact_owner_ids.as_mut_slice()[position] =
                                            FactId::try_from_index(cursor).ok_or(DataError::ArithmeticOverflow)?;
                                        self.counts.as_mut_slice()[index] =
                                            position.checked_add(1).ok_or(DataError::ArithmeticOverflow)?;
                                    }
                                }
                                FinalizationPhase::IndexFactOwners {
                                    step: FactIndexStep::Fill { fact: fact + granted },
                                }
                            }
                        }
                    }
                }
                FinalizationPhase::Publish => {
                    if resources.admit_work_transition()? == jqf_resource::WorkAdmission::Pending {
                        self.phase = FinalizationPhase::Publish;
                        return Ok(DocumentFinalizationPoll::Pending);
                    }
                    resources.check_control()?;
                    let mut builder = self.builder.take().ok_or(DataError::InvalidDocument)?;
                    // The staging blocks flushed at the first poll and nothing refills them, so their capacity is
                    // already dead weight on a builder that is about to drop: park it for the next document.
                    super::transients::park(&mut self.spare.staged_nodes, &mut builder.staged_nodes);
                    super::transients::park(&mut self.spare.staged_occurrences, &mut builder.staged_occurrences);
                    // Move the node table out so the arena pass can mutate its `projection_range` while `&builder`
                    // still resolves key text.
                    let mut nodes = core::mem::take(&mut builder.nodes);
                    let coverage = published_coverage(
                        builder.demanded_coverage,
                        builder.empty_families,
                        builder.diagnostics,
                        &CoverageEvidence {
                            nodes: nodes.as_slice(),
                            wide: builder.wide.as_slice(),
                            occurrences: builder.occurrences.as_slice(),
                            facts: builder.facts.as_slice(),
                            authored_spans: builder.authored_spans.as_slice(),
                        },
                    )?;
                    let arenas = super::storage::emit_relationship_arenas_accounted(
                        nodes.as_mut_slice(),
                        builder.occurrences.as_slice(),
                        self.owner_occurrences.as_slice(),
                        builder.demanded_coverage.topology(),
                        &builder,
                    )?;
                    // `owner_occurrences` fed the arena pass above and nothing past it reads that adjacency scratch
                    // (mirroring `owner_positions`'s release at the top of this poll): release it now instead of
                    // holding it live for the rest of publication and past `Ready`.
                    super::transients::park(&mut self.spare.owner_occurrences, &mut self.owner_occurrences);
                    let (shared_schema, inline_schema) = match builder.shared_schema {
                        Some(schema) => (Some(schema), None),
                        None => (
                            None,
                            Some(builder.schema.take().ok_or(DataError::InvalidDocument)?.finish()),
                        ),
                    };
                    match (&shared_schema, &inline_schema) {
                        (Some(schema), None)
                            if schema.validate_published_records(
                                nodes.as_slice(),
                                builder.occurrences.as_slice(),
                                builder.facts.as_slice(),
                            ) => {}
                        (None, Some(schema))
                            if schema.validate()
                                && schema.validate_published_records(
                                    nodes.as_slice(),
                                    builder.occurrences.as_slice(),
                                    builder.facts.as_slice(),
                                ) => {}
                        (Some(_) | None, None | Some(_)) => {
                            return Err(DataError::InvalidDocument);
                        }
                    }
                    // Amortized append growth may have retained more capacity than the published text needs; release
                    // that headroom.
                    builder.stored_text.shrink_to_fit();
                    builder.wide.shrink_to_fit();
                    builder.tags.shrink_to_fit();
                    builder.facts.shrink_to_fit();
                    // The node table is the only build arena published directly. A single-pass codec grows it either by
                    // amortized doubling or from a projected reservation, so release that headroom here.
                    nodes.shrink_to_fit();
                    let storage = DocumentStorage {
                        #[cfg(feature = "benchmark-internals")]
                        schema_execution: builder.schema_execution,
                        format: builder.format,
                        dialect: builder.dialect,
                        key: builder.key,
                        root: self.root,
                        coverage,
                        text: super::DocumentTextStorage::new(builder.source_binding, builder.stored_text),
                        _source: PhantomData,
                        nodes,
                        wide: builder.wide,
                        tags: builder.tags,
                        facts: builder.facts,
                        fact_owner_index: super::storage::FactOwnerIndex {
                            nodes: core::mem::take(&mut self.fact_owner_nodes),
                            ranges: core::mem::take(&mut self.fact_owner_ranges),
                            fact_ids: core::mem::take(&mut self.fact_owner_ids),
                        },
                        fact_owner_indexed: true,
                        edges: arenas.edges,
                        sidecars: arenas.sidecars,
                        keys: arenas.keys,
                        edge_refs: arenas.edge_refs,
                        winners: arenas.winners,
                        lookup: arenas.lookup,
                        relationship_total: arenas.relationship_total,
                        span_materializer: builder.span_materializer,
                        container_spans: builder.container_spans,
                        authored_spans: builder.authored_spans,
                    };
                    let storage = match (shared_schema, inline_schema) {
                        (Some(schema), None) => DocumentStorageOwner::new_accounted_shared(schema, storage),
                        (None, Some(schema)) => DocumentStorageOwner::new_accounted_inline(schema, storage),
                        (Some(_), Some(_)) | (None, None) => {
                            return Err(DataError::InvalidDocument);
                        }
                    };
                    self.phase = FinalizationPhase::Complete;
                    return Ok(DocumentFinalizationPoll::Ready(Document {
                        storage,
                        borrowed_source: None,
                        trusted_session_source_attachment: false,
                        source_canonical: false,
                    }));
                }
                // Polling again once the finalizer already reached its terminal phase is the same "already-terminal,
                // cannot resume" condition `poll_with_source` reports and every other cooperative reader in this crate
                // (`ReaderState::Failed`, `SourceAttachmentPhase::Complete | Failed`, ...) reports via `ReaderFailed`,
                // not `InvalidDocument` (reserved for actual structural invalidity).
                FinalizationPhase::Complete => return Err(DataError::ReaderFailed),
            };
        }
    }
}

/// Copies a local time into owned storage; the fraction digits are the only heap-owning field.
pub(super) fn copy_accounted_local_time(value: &crate::LocalTime) -> Result<AccountedLocalTime, DataError> {
    Ok(AccountedLocalTime {
        hour: value.hour(),
        minute: value.minute(),
        second: value.second(),
        fraction: crate::identity::try_copy_str(value.fraction().digits())?,
    })
}

/// Copies a local date-time: the date is `Copy` and passes through; the time's fraction digits are copied.
pub(super) fn copy_accounted_local_date_time(
    value: &crate::LocalDateTime,
) -> Result<AccountedLocalDateTime, DataError> {
    Ok(AccountedLocalDateTime {
        date: value.date,
        time: copy_accounted_local_time(&value.time)?,
    })
}

/// Copies an offset date-time: the offset is `Copy` and passes through; the local date-time's fraction digits are
/// copied.
pub(super) fn copy_accounted_offset_date_time(
    value: &crate::OffsetDateTime,
) -> Result<AccountedOffsetDateTime, DataError> {
    Ok(AccountedOffsetDateTime {
        local: copy_accounted_local_date_time(&value.local)?,
        offset: value.offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DocumentSchemaRecipe, PreparedSemanticNode};

    #[test]
    fn detached_source_never_widens_observable_source_coverage() {
        let source = jqf_source::SourceRef::new(jqf_source::SourceId::new(41), jqf_source::SourceKind::Input);
        let bytes = b"value";
        let resolved = jqf_source::ResolvedSource::new(source, source.kind().as_str(), bytes, 0);
        let control = jqf_resource::ContinueControl;
        let limits = jqf_resource::ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let account = jqf_resource::RequestAccount::try_new(limits).expect("account allocates");
        let work = jqf_resource::WorkMeter::try_new_v1(4096).expect("work meter starts");
        let mut resources = jqf_resource::ResourceContext::new(account, &control, work).expect("context starts");
        let recipe =
            DocumentSchemaRecipe::try_new("test", None, &["test.string"], &[], &[], &[]).expect("recipe is valid");
        let (mut builder, prepared) =
            AccountedDocumentBuilder::try_new_prepared_with_coverage(&recipe, crate::BuilderCoverage::complete())
                .expect("detached builder starts");
        builder
            .bind_source(super::super::DocumentSourceBinding::from_resolved(resolved).expect("binding seals source"))
            .expect("binding attaches to builder");
        // SAFETY: the span names the whole retained immutable source, which
        // this test keeps unchanged through publication and attachment.
        let root = unsafe {
            builder.add_prepared_bound_source_string_node(
                &prepared,
                prepared.node_kind(0).expect("prepared string kind exists"),
                jqf_source::Span::from_usize(0, bytes.len()),
                &resources,
            )
        }
        .expect("source-backed root is added");
        let mut finalizer = builder.begin_finish(root, &resources).expect("finalizer starts");
        let detached = loop {
            match finalizer.poll(&mut resources).expect("finalizer polls") {
                DocumentFinalizationPoll::Pending => {
                    assert!(
                        resources
                            .try_begin_next_cooperative_entry(4096)
                            .expect("next cooperative entry starts")
                    );
                }
                DocumentFinalizationPoll::Ready(document) => break document,
            }
        };
        // `owner_occurrences` is released once the arena pass has consumed it, well before `Ready`, rather than held
        // for the rest of publication.
        assert_eq!(finalizer.owner_occurrences.capacity(), 0);
        // Polling a finalizer already at its terminal phase reports the same `ReaderFailed` every other terminal
        // cooperative reader in this crate reports, not `InvalidDocument`.
        assert!(matches!(finalizer.poll(&mut resources), Err(DataError::ReaderFailed)));
        assert!(
            !detached
                .coverage()
                .contains(super::super::DocumentCapability::WholeSource)
        );

        let trusted_misuse_document = detached.try_clone().expect("trusted misuse clone");

        // SAFETY: this test retains the exact immutable authority used for the
        // binding and every source token through finalization and attachment.
        let trusted = unsafe { detached.with_borrowed_source_from_bound_authority(resolved, &resources) }
            .expect("trusted attachment succeeds");
        assert!(
            trusted
                .text_storage_stats()
                .expect("trusted attachment stats")
                .trusted_session_source_attachment
        );
        assert!(
            !trusted
                .coverage()
                .contains(super::super::DocumentCapability::WholeSource)
        );
        assert!(matches!(
            trusted.materialize_root(&mut resources),
            Ok(crate::Value::String(value)) if value == "value"
        ));

        let wrong_ref = jqf_source::SourceRef::new(jqf_source::SourceId::new(42), jqf_source::SourceKind::Input);
        let wrong_metadata = jqf_source::ResolvedSource::new(wrong_ref, wrong_ref.kind().as_str(), bytes, 0);
        // SAFETY: the call deliberately supplies detectable wrong metadata;
        // the boundary must reject it before installing backing.
        assert!(matches!(
            unsafe { trusted_misuse_document.with_borrowed_source_from_bound_authority(wrong_metadata, &resources) },
            Err(DataError::InvalidDocument)
        ));

        let already_attached = trusted.try_clone().expect("attached clone");
        // SAFETY: source continuity is valid, but the lifecycle contract must
        // reject a second attachment to an already attached document.
        assert!(matches!(
            unsafe { already_attached.with_borrowed_source_from_bound_authority(resolved, &resources) },
            Err(DataError::InvalidDocument)
        ));
    }
    #[test]
    fn finalizer_falls_back_to_the_counting_sort_for_non_stack_authoring() {
        // The single-pass adjacency pass classifies each occurrence against the open-frame markers; authoring "A, B, A,
        // B" reopens owner B after its frame was flushed by the Return step, which no stack discipline can represent.
        // The finalizer must then fall back to the order-agnostic counting-sort and still publish a correct document.
        let limits = jqf_resource::ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let control = jqf_resource::ContinueControl;
        let account = jqf_resource::RequestAccount::try_new(limits).expect("account");
        let work = jqf_resource::WorkMeter::try_new_v1(4096).expect("work");
        let mut resources = jqf_resource::ResourceContext::new(account, &control, work).expect("context");
        let recipe =
            DocumentSchemaRecipe::try_new("test", None, &["test.array", "test.scalar"], &["test.item"], &[], &[])
                .expect("recipe is valid");
        let (mut builder, prepared) =
            AccountedDocumentBuilder::try_new_prepared_with_coverage(&recipe, crate::BuilderCoverage::complete())
                .expect("prepared builder starts");
        let array_kind = prepared.node_kind(0).expect("array kind");
        let scalar_kind = prepared.node_kind(1).expect("scalar kind");
        let item_role = prepared.occurrence_role(0).expect("item role");
        let mut nodes = alloc::vec::Vec::new();
        for _ in 0..4 {
            nodes.push(
                builder
                    .add_prepared_node(&prepared, scalar_kind, PreparedSemanticNode::Null, &resources)
                    .expect("scalar node"),
            );
        }
        let array_a = builder
            .add_prepared_node(
                &prepared,
                array_kind,
                PreparedSemanticNode::Array(item_role),
                &resources,
            )
            .expect("array A");
        let array_b = builder
            .add_prepared_node(
                &prepared,
                array_kind,
                PreparedSemanticNode::Array(item_role),
                &resources,
            )
            .expect("array B");
        // A, B, A, B: the second B reopens a flushed owner.
        for (owner, target) in [
            (array_a, nodes[0]),
            (array_b, nodes[1]),
            (array_a, nodes[2]),
            (array_b, nodes[3]),
        ] {
            builder
                .add_prepared_occurrence(
                    &prepared,
                    LocalOwnerRef::Node(owner),
                    item_role,
                    None,
                    target,
                    &resources,
                )
                .expect("occurrence attaches");
        }
        let mut finalizer = builder.begin_finish(array_a, &resources).expect("finalizer starts");
        let document = loop {
            match finalizer.poll(&mut resources).expect("finalizer polls") {
                DocumentFinalizationPoll::Pending => {
                    resources
                        .try_begin_next_cooperative_entry(4096)
                        .expect("next cooperative entry");
                }
                DocumentFinalizationPoll::Ready(document) => break document,
            }
        };
        // The fallback preserves each owner's members in authored order.
        let root = document.materialize_root(&mut resources).expect("root");
        let crate::Value::Array(items) = &root else {
            panic!("root array materialized");
        };
        assert_eq!(items.len(), 2);
        let values: alloc::vec::Vec<_> = items.iter().map(crate::Value::kind).collect();
        assert_eq!(values, alloc::vec![crate::ValueKind::Null, crate::ValueKind::Null]);
    }

    #[test]
    fn begin_finish_builds_the_fact_owner_index() {
        let limits = jqf_resource::ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let control = jqf_resource::ContinueControl;
        let account = jqf_resource::RequestAccount::try_new(limits).expect("account");
        let work = jqf_resource::WorkMeter::try_new_v1(4096).expect("work");
        let mut resources = jqf_resource::ResourceContext::new(account, &control, work).expect("context");
        let recipe = DocumentSchemaRecipe::try_new("test", None, &["test.kind"], &[], &["test.role"], &[])
            .expect("recipe is valid");
        let (mut builder, prepared) =
            AccountedDocumentBuilder::try_new_prepared_with_coverage(&recipe, crate::BuilderCoverage::complete())
                .expect("prepared builder starts");
        let first = builder
            .add_prepared_node(
                &prepared,
                prepared.node_kind(0).expect("kind"),
                PreparedSemanticNode::Null,
                &resources,
            )
            .expect("first node");
        let second = builder
            .add_prepared_node(
                &prepared,
                prepared.node_kind(0).expect("kind"),
                PreparedSemanticNode::Null,
                &resources,
            )
            .expect("second node");
        for (node, text) in [(first, "a"), (first, "c"), (second, "b")] {
            builder
                .add_fact(
                    LocalOwnerRef::Node(node),
                    "test.role",
                    "test.kind",
                    1,
                    &crate::FactPayload::Text(alloc::string::String::from(text)),
                    &resources,
                )
                .expect("fact attaches");
        }
        let mut finalizer = builder.begin_finish(second, &resources).expect("finalizer starts");
        let document = loop {
            match finalizer.poll(&mut resources).expect("finalizer polls") {
                DocumentFinalizationPoll::Pending => {
                    resources
                        .try_begin_next_cooperative_entry(4096)
                        .expect("next cooperative entry");
                }
                DocumentFinalizationPoll::Ready(document) => break document,
            }
        };
        // The index's content law: per-node contiguous ranges, ids ascending within a node, and no overlap — the
        // counting-sort phases (`Assign`/ `Fill`) are the machinery a length-only check would miss.
        let first_ids = document.owner_fact_ids(first);
        assert_eq!(first_ids.len(), 2);
        assert!(
            first_ids.windows(2).all(|pair| pair[0] < pair[1]),
            "a node's fact ids ascend within its contiguous range"
        );
        let first_fact = document.fact(first_ids[0]).expect("first fact resolves");
        assert_eq!(first_fact.kind().as_str(), "test.kind");
        let second_ids = document.owner_fact_ids(second);
        assert_eq!(second_ids.len(), 1);
        assert!(
            second_ids.iter().all(|id| !first_ids.contains(id)),
            "per-node ranges never overlap"
        );
        let fact = document.fact(second_ids[0]).expect("second fact resolves");
        assert_eq!(fact.kind().as_str(), "test.kind");
        // A node that owns no facts reads an empty slice.
        assert!(
            document
                .owner_fact_ids(crate::NodeId::try_from_index(9).expect("node nine"))
                .is_empty()
        );
        let role = document.fact_role_binding("test.role").expect("role interned");
        let kind = document.fact_kind_binding("test.kind").expect("kind interned");
        let view = document
            .owner_fact_payload_in(first, core::slice::from_ref(&role), Some(kind))
            .expect("payload view without cloning FactPayload");
        let Some(crate::FactPayloadView::Text(text)) = view else {
            panic!("text payload");
        };
        assert_eq!(text, "a");
        assert_eq!(document.fact(first_ids[0]).expect("fact").role_binding(), role);
    }

    #[test]
    fn finish_builds_the_fact_owner_index() {
        let limits = jqf_resource::ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let control = jqf_resource::ContinueControl;
        let account = jqf_resource::RequestAccount::try_new(limits).expect("account");
        let work = jqf_resource::WorkMeter::try_new_v1(4096).expect("work");
        let resources = jqf_resource::ResourceContext::new(account, &control, work).expect("context");
        let recipe = DocumentSchemaRecipe::try_new(
            "test",
            None,
            &["test.kind"],
            &[],
            &["comment", "noise"],
            &["comment", "noise"],
        )
        .expect("recipe is valid");
        let (mut builder, prepared) =
            AccountedDocumentBuilder::try_new_prepared_with_coverage(&recipe, crate::BuilderCoverage::complete())
                .expect("prepared builder starts");
        let commented = builder
            .add_prepared_node(
                &prepared,
                prepared.node_kind(0).expect("kind"),
                PreparedSemanticNode::Null,
                &resources,
            )
            .expect("commented node");
        let other = builder
            .add_prepared_node(
                &prepared,
                prepared.node_kind(0).expect("kind"),
                PreparedSemanticNode::Null,
                &resources,
            )
            .expect("other node");
        builder
            .add_fact(
                LocalOwnerRef::Node(commented),
                "comment",
                "comment",
                1,
                &crate::FactPayload::List(alloc::vec![crate::FactPayload::Text(alloc::string::String::from(
                    "note"
                ))]),
                &resources,
            )
            .expect("comment fact");
        builder
            .add_fact(
                LocalOwnerRef::Node(other),
                "noise",
                "noise",
                1,
                &crate::FactPayload::Text(alloc::string::String::from("x")),
                &resources,
            )
            .expect("noise fact");
        let document = builder.finish(other, &resources).expect("finish");
        assert!(
            document.fact_owner_indexed(),
            "finish builds the same owner index the cooperative path does"
        );
        let comment_ids = document.owner_fact_ids(commented);
        assert_eq!(comment_ids.len(), 1);
        let fact = document.fact(comment_ids[0]).expect("comment fact");
        assert_eq!(fact.role().as_str(), "comment");
        assert_eq!(document.owner_fact_ids(other).len(), 1);
        assert!(
            document
                .owner_fact_ids(crate::NodeId::try_from_index(9).expect("node nine"))
                .is_empty()
        );
        let empty =
            AccountedDocumentBuilder::try_new_prepared_with_coverage(&recipe, crate::BuilderCoverage::complete())
                .expect("empty builder");
        let (mut empty, prepared) = empty;
        let root = empty
            .add_prepared_node(
                &prepared,
                prepared.node_kind(0).expect("kind"),
                PreparedSemanticNode::Null,
                &resources,
            )
            .expect("root");
        let empty = empty.finish(root, &resources).expect("empty finish");
        assert!(empty.fact_owner_indexed());
        assert!(empty.owner_fact_ids(root).is_empty());
    }
}
