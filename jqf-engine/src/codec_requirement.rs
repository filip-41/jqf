//! Exact engine-analysis lowering into format-neutral codec requirements.

use jqf_codec_core::{
    AccessFootprint, AccessGuarantees, AccessRequirement, CodecDemand, CodecError, DemandClause, DiagnosticPolicy,
    ExactPath, PruneTree, PruneTreeError, ValidationMode,
};
use jqf_resource::ResourceContext;

/// Decoder policy axes which engine analysis must preserve during lowering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CodecRequirementPolicy {
    validation: ValidationMode,
    diagnostics: DiagnosticPolicy,
    /// Explicit container-span frontier override (the CLI's
    /// `JQF_CONTAINER_SPANS` env var). `None` lets the engine's root lowering decide from the
    /// program's consumption class.
    lazy_frontier: Option<u32>,
}

impl CodecRequirementPolicy {
    /// Creates the policy shared by a decoder request and its engine requirement.
    #[must_use]
    pub const fn new(validation: ValidationMode, diagnostics: DiagnosticPolicy) -> Self {
        Self {
            validation,
            diagnostics,
            lazy_frontier: None,
        }
    }

    /// The explicit frontier override, when the caller named one.
    #[must_use]
    pub const fn lazy_frontier(self) -> Option<u32> {
        self.lazy_frontier
    }

    /// Builder: names an explicit container-span frontier depth that overrides
    /// the engine's program-class decision (the force-lazy differential's env
    /// var flows through here).
    #[must_use]
    pub const fn with_lazy_frontier(mut self, depth: u32) -> Self {
        self.lazy_frontier = Some(depth);
        self
    }

    /// Required input validation guarantee.
    #[must_use]
    pub const fn validation(self) -> ValidationMode {
        self.validation
    }

    /// Required structured diagnostic coverage.
    #[must_use]
    pub const fn diagnostics(self) -> DiagnosticPolicy {
        self.diagnostics
    }
}

/// One decoded component of an engine-known static forward path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StaticForwardStep<'path> {
    /// A decoded UTF-8 object key.
    ObjectKey(&'path str),
    /// A signed array position preserving the negative-index semantics.
    ArrayIndex(i64),
    /// A contiguous element RANGE over an array container, normalized to signed
    /// `i64`-or-open bounds at lower time (the boundary law,
    /// `SliceBounds::try_normalize`). The CODEC resolves the bounds against the
    /// observed container length, exactly as it already resolves a signed
    /// [`Self::ArrayIndex`].
    ArrayRange {
        /// Inclusive start, or `None` for the container's start.
        start: Option<i64>,
        /// Exclusive end, or `None` for the container's end.
        end: Option<i64>,
    },
}

/// Lowers a complete semantic input read into one whole-document requirement.
///
/// `lazy_frontier` is an explicit container-span frontier override; `None`
/// takes the engine default, which is LAZY at depth 1 — the whole-document
/// route's shipped shape. This is the ONE place the default
/// lives: every other caller either passes `None` and inherits it, or passes
/// `Some(depth)` because it knows the build should differ. The corrupt-late
/// law is untouched (the codec's validating scan still visits every byte;
/// only container NODE construction defers), and a subtree nothing touches
/// costs its span record and nothing else.
///
/// A caller that provably materializes the whole document anyway (the
/// `-n`/`-s`/edit/FFI decode helpers) passes `Some(0)` to keep the eager
/// build, and the prune interaction is decided by the caller — the only
/// place that knows the tree.
pub fn try_lower_root_requirement(
    policy: CodecRequirementPolicy,
    lazy_frontier: Option<u32>,
    resources: &ResourceContext<'_>,
) -> Result<AccessRequirement, CodecError> {
    Ok(AccessRequirement::try_whole(
        semantic_demand(resources)?,
        AccessGuarantees::new(policy.validation(), policy.diagnostics()),
        resources,
    )?
    .with_lazy_frontier(lazy_frontier.unwrap_or(1)))
}

/// Lowers one engine-known static path into an exact located requirement.
///
/// Object keys are already decoded semantic strings. An empty path is the
/// canonical exact root selection and is deliberately distinct from complete
/// document access.
pub fn try_lower_forward_requirement(
    policy: CodecRequirementPolicy,
    path: &[StaticForwardStep<'_>],
    resources: &ResourceContext<'_>,
) -> Result<AccessRequirement, CodecError> {
    let exact = build_exact_path(path, resources)?;
    let footprint = AccessFootprint::try_exact(exact, resources);
    AccessRequirement::try_exact(
        footprint,
        semantic_demand(resources)?,
        AccessGuarantees::new(policy.validation(), policy.diagnostics()),
        resources,
    )
}

/// Lowers the analysis walk's recursive kept-subtree tree into the flat
/// transported [`PruneTree`]. Children are minted BEFORE the parent's edges
/// name them (the transport validates child ids at push time), and the walk's
/// `BTreeMap` iteration order satisfies the transport's ascending-key contract
/// by construction.
pub(crate) fn try_lower_prune_tree(
    tree: &crate::analysis::PruneNode,
    resources: &ResourceContext<'_>,
) -> Result<PruneTree, CodecError> {
    let mut transport = PruneTree::try_new(resources).map_err(prune_error)?;
    lower_prune_into(&mut transport, PruneTree::ROOT, tree).map_err(prune_error)?;
    Ok(transport)
}

fn lower_prune_into(
    transport: &mut PruneTree,
    node_id: u32,
    node: &crate::analysis::PruneNode,
) -> Result<(), PruneTreeError> {
    if let Some(element) = node.element() {
        let child = transport.try_push_node(element.is_all())?;
        lower_prune_into(transport, child, element)?;
        transport.try_set_element(node_id, child)?;
    }
    for (name, member) in node.keys() {
        let child = transport.try_push_node(member.is_all())?;
        lower_prune_into(transport, child, member)?;
        transport.try_push_key(node_id, name, child)?;
    }
    Ok(())
}

/// Maps a prune-tree rejection onto the codec error channel. The non-resource
/// arms are engine bugs — the walk's budget sits below the transport's node
/// bound and the `BTreeMap` order satisfies the key contract — so they surface
/// as contract violations rather than as declines.
fn prune_error(error: PruneTreeError) -> CodecError {
    match error {
        PruneTreeError::Resource(error) => CodecError::from(error),
        PruneTreeError::TooManyNodes | PruneTreeError::UnorderedKey | PruneTreeError::UnknownNode => {
            CodecError::new(jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "prune-tree lowering violated the transport contract",
            })
        }
    }
}

/// Builds the codec exact path for one engine-known static forward path.
fn build_exact_path(path: &[StaticForwardStep<'_>], resources: &ResourceContext<'_>) -> Result<ExactPath, CodecError> {
    let mut exact = ExactPath::try_new(resources);
    for step in path {
        match step {
            StaticForwardStep::ObjectKey(key) => {
                exact.try_push_semantic_member(key, resources)?;
            }
            StaticForwardStep::ArrayIndex(index) => {
                exact.try_push_semantic_index(*index, resources);
            }
            StaticForwardStep::ArrayRange { start, end } => {
                exact.try_push_semantic_range(*start, *end, resources);
            }
        }
    }
    Ok(exact)
}

fn semantic_demand(resources: &ResourceContext<'_>) -> Result<CodecDemand, CodecError> {
    let mut demand = CodecDemand::try_new(resources);
    demand.try_insert(&DemandClause::SemanticRoot)?;
    demand.try_insert(&DemandClause::ValueShape)?;
    Ok(demand)
}

#[cfg(test)]
mod tests {
    use super::{CodecRequirementPolicy, StaticForwardStep, try_lower_forward_requirement, try_lower_root_requirement};
    use jqf_codec_core::{AccessResultKind, DiagnosticPolicy, PortableStep, ValidationMode};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("resources")
    }

    fn policy() -> CodecRequirementPolicy {
        CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly)
    }

    #[test]
    fn whole_root_and_exact_root_are_distinct_contracts() {
        let resources = resources();
        let whole = try_lower_root_requirement(policy(), Some(0), &resources).expect("whole");
        assert!(whole.footprint().is_whole());
        assert!(whole.schedule().is_empty_complete());
        assert_eq!(whole.result(), AccessResultKind::CompleteDocument);

        let exact = try_lower_forward_requirement(policy(), &[], &resources).expect("exact root");
        assert!(exact.footprint().exact_path().expect("path").is_root());
        assert!(!exact.schedule().is_empty_complete());
        assert_eq!(exact.result(), AccessResultKind::Located);
    }

    #[test]
    fn static_path_preserves_decoded_members_and_signed_indices() {
        let resources = resources();
        let requirement = try_lower_forward_requirement(
            policy(),
            &[StaticForwardStep::ObjectKey("items"), StaticForwardStep::ArrayIndex(-1)],
            &resources,
        )
        .expect("requirement");
        let steps = requirement.footprint().exact_path().expect("exact").steps();
        assert!(matches!(&steps[0], PortableStep::SemanticMember(m) if m == "items"));
        assert!(matches!(steps[1], PortableStep::SemanticIndex(-1)));
    }
}
