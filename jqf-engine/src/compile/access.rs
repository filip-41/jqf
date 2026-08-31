//! Packed codec access. Finish stores [`AccessPlan`]; [`CompiledProgram::try_requirement`]
//! charges it. Codecs see one shape; they do not re-walk demands.
//!
//! Identity reuses whole-document access. A static forward path reuses
//! exact-path access. Identity never lowers as a forward requirement with an
//! empty path. A non-lenient mismatch policy still rebinds Whole so engine
//! cells fire.

#[allow(clippy::wildcard_imports)]
use super::*;

/// One static path step owned by the packed plan.
#[derive(Clone, Debug, Eq, PartialEq)]
enum PackedStep {
    ObjectKey(String),
    ArrayIndex(i64),
    ArrayRange { start: Option<i64>, end: Option<i64> },
}

impl PackedStep {
    fn as_forward(&self) -> StaticForwardStep<'_> {
        match self {
            Self::ObjectKey(key) => StaticForwardStep::ObjectKey(key),
            Self::ArrayIndex(index) => StaticForwardStep::ArrayIndex(*index),
            Self::ArrayRange { start, end } => StaticForwardStep::ArrayRange {
                start: *start,
                end: *end,
            },
        }
    }
}

/// `.@tag` / `.@role` / `.&name` / xpath-css topology, scanned once at finish.
#[derive(Clone, Debug, Default)]
struct PackedClauses {
    want_tag: bool,
    want_all_facts: bool,
    want_topology: bool,
    facts: Vec<String>,
    attributes: Vec<String>,
}

/// What codecs bind. Packed at finish; [`CompiledProgram::try_requirement`] charges it.
#[derive(Debug)]
pub(crate) struct AccessPlan {
    /// Whole or Exact. Empty prefix is Whole.
    pub kind: Access,
    path: Vec<PackedStep>,
    /// Exact charge returns the prefix-contract error when the packed path was unusable.
    path_unusable: bool,
    frontier: Option<u32>,
    prune: Option<crate::analysis::PruneNode>,
    /// Document-rooted prune for [`Self::charge_whole_rebind`]. Exact count and
    /// element store a container-rooted `prune`; rebound Whole must keep `.users`.
    rebind_prune: Option<crate::analysis::PruneNode>,
    count: Option<jqf_data::CountDemand>,
    element: Option<jqf_data::ElementDemand>,
    has_key: Option<jqf_data::Value>,
    keys: bool,
    type_demand: bool,
    element_construct: Option<crate::analysis::element::ConstructFields>,
    minmax: Option<jqf_data::MinMaxHint>,
    clauses: PackedClauses,
    range_path: Option<Vec<PackedStep>>,
}

fn insert_clause(requirement: &mut AccessRequirement, clause: &DemandClause) -> Result<(), CodecError> {
    requirement.try_insert_clause(clause).map_err(CodecError::from)
}

fn pack_count_path(path: &[jqf_data::CountStep]) -> Vec<PackedStep> {
    path.iter()
        .map(|step| match step {
            jqf_data::CountStep::ObjectKey(key) => PackedStep::ObjectKey(key.clone()),
            jqf_data::CountStep::ArrayIndex(index) => PackedStep::ArrayIndex(*index),
        })
        .collect()
}

/// Kept-subtree prune rooted at the located container: drop `demand.path`,
/// keep `with_element(probe)`. YAML Exact prune is the located subtree.
fn count_prune_at_container(demand: &jqf_data::CountDemand) -> Option<crate::analysis::PruneNode> {
    let mut at_container = demand.clone();
    at_container.path.clear();
    crate::analysis::count::count_prune_tree(&at_container)
}

fn consumer_frontier(policy: CodecRequirementPolicy, stays_lazy: bool) -> u32 {
    match policy.lazy_frontier() {
        Some(depth) => depth,
        // Concrete 1, not `None`: Whole's root lowerer treats `None` as 1, but
        // Exact's default is 0. Pack the number charge will apply.
        None if stays_lazy => 1,
        None => 0,
    }
}

fn pack_pushdown(program: &Program) -> Result<Vec<PackedStep>, ()> {
    let steps = program.entry_stage_steps();
    let prefix = &steps[..program.split().prefix_len()];
    let mut path = Vec::new();
    path.try_reserve_exact(prefix.len()).map_err(|_| ())?;
    for step in prefix {
        path.push(match step.access() {
            StepAccess::Key(key) => PackedStep::ObjectKey(String::from(key.as_str())),
            StepAccess::Index(index) => PackedStep::ArrayIndex(*index),
            StepAccess::Each
            | StepAccess::DynVar(_)
            | StepAccess::Descend
            | StepAccess::Slice(_)
            | StepAccess::NodeAccessor(_)
            | StepAccess::Attribute(_)
            | StepAccess::DynNodeAccessor(_)
            | StepAccess::DynAttribute(_) => return Err(()),
        });
    }
    Ok(path)
}

fn residual_starts_at_each(program: &Program) -> bool {
    let steps = program.entry_stage_steps();
    matches!(
        steps.get(program.split().prefix_len()).map(StageStep::access),
        Some(StepAccess::Each)
    )
}

fn walk_prune<'a>(
    tree: Option<&'a crate::analysis::PruneNode>,
    path: &[PackedStep],
) -> Option<&'a crate::analysis::PruneNode> {
    let mut node = tree?;
    for step in path {
        if node.is_all() {
            return None;
        }
        match step {
            PackedStep::ObjectKey(key) => {
                node = node.keys().get(key.as_str())?;
            }
            PackedStep::ArrayIndex(_) => {
                node = node.element()?;
            }
            PackedStep::ArrayRange { .. } => {
                return None;
            }
        }
    }
    if node.is_all() {
        return None;
    }
    Some(node)
}

fn reanchor_prune<'a>(
    program: &Program,
    tree: Option<&'a crate::analysis::PruneNode>,
    path: &[PackedStep],
) -> Option<&'a crate::analysis::PruneNode> {
    if residual_starts_at_each(program) {
        return None;
    }
    walk_prune(tree, path)
}

fn pack_range_path(program: &Program, shortcut: &Shortcut) -> Option<Vec<PackedStep>> {
    if !matches!(shortcut, Shortcut::RangeLocate) {
        return None;
    }
    let steps = program.stage_steps(program.root());
    let mut path = Vec::new();
    path.try_reserve_exact(steps.len()).ok()?;
    for step in steps {
        path.push(match step.access() {
            StepAccess::Key(key) => PackedStep::ObjectKey(String::from(key.as_str())),
            StepAccess::Index(index) => PackedStep::ArrayIndex(*index),
            StepAccess::Slice(bounds) => {
                let (start, end) = bounds.try_normalize()?;
                PackedStep::ArrayRange { start, end }
            }
            StepAccess::Each
            | StepAccess::DynVar(_)
            | StepAccess::Descend
            | StepAccess::NodeAccessor(_)
            | StepAccess::Attribute(_)
            | StepAccess::DynNodeAccessor(_)
            | StepAccess::DynAttribute(_) => return None,
        });
    }
    Some(path)
}

fn scan_clauses(program: &Program) -> PackedClauses {
    let mut clauses = PackedClauses::default();
    for node in program.nodes() {
        if let ProgramNode::Call { overload, .. } = node
            && (overload.get() == jqf_builtins::registry::builtins::id::XPATH_1
                || overload.get() == jqf_builtins::registry::builtins::id::CSS_1)
        {
            clauses.want_all_facts = true;
            clauses.want_topology = true;
        }
        let ProgramNode::Stage { steps, .. } = node else {
            continue;
        };
        for step in steps {
            match step.access() {
                StepAccess::NodeAccessor(name) if name == "tag" => clauses.want_tag = true,
                StepAccess::NodeAccessor(name) => clauses.facts.push(String::from(name.as_str())),
                StepAccess::DynNodeAccessor(_) => {
                    clauses.want_tag = true;
                    clauses.want_all_facts = true;
                }
                StepAccess::Attribute(name) => clauses.attributes.push(String::from(name.as_str())),
                StepAccess::DynAttribute(_) => clauses.want_all_facts = true,
                _ => {}
            }
        }
    }
    clauses
}

fn attached_fact(role: &str) -> Result<DemandClause, CodecError> {
    DemandClause::try_attached_fact(role).ok_or_else(|| {
        CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "attached-fact demand identity",
        })
    })
}

impl PackedClauses {
    fn attach(&self, requirement: &mut AccessRequirement) -> Result<(), CodecError> {
        if self.want_tag {
            insert_clause(requirement, &DemandClause::IntrinsicTag)?;
        }
        if self.want_all_facts {
            for role in jqf_codec_core::ATTACHED_FACT_ROLES {
                insert_clause(requirement, &attached_fact(role)?)?;
            }
        } else {
            for role in &self.facts {
                if !jqf_codec_core::ATTACHED_FACT_ROLES.contains(&role.as_str()) {
                    continue;
                }
                insert_clause(requirement, &attached_fact(role)?)?;
            }
        }
        for name in &self.attributes {
            let expanded = ExpandedName::try_new("", name).map_err(|_| {
                CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "attribute demand identity",
                })
            })?;
            insert_clause(requirement, &DemandClause::Attribute(expanded))?;
        }
        if self.want_topology {
            insert_clause(
                requirement,
                &DemandClause::Topology(jqf_codec_core::TopologyDemand::Children),
            )?;
            insert_clause(
                requirement,
                &DemandClause::Topology(jqf_codec_core::TopologyDemand::Owner),
            )?;
        }
        Ok(())
    }
}

impl AccessPlan {
    /// Packs the codec shape finish committed. The demand if-ladder lives here,
    /// once; [`CompiledProgram::try_requirement`] only charges.
    pub(crate) fn pack(
        program: &Program,
        shortcut: &Shortcut,
        access: Access,
        prune: Option<&crate::analysis::PruneNode>,
        policy: CodecRequirementPolicy,
        consumes_whole: bool,
    ) -> Self {
        let clauses = scan_clauses(program);
        let range_path = pack_range_path(program, shortcut);
        match access {
            Access::Whole => pack_whole(shortcut, prune, policy, consumes_whole, clauses, range_path),
            Access::Exact => pack_exact(program, shortcut, access, prune, policy, clauses, range_path),
        }
    }

    fn charge(
        &self,
        policy: CodecRequirementPolicy,
        resources: &ResourceContext<'_>,
    ) -> Result<AccessRequirement, CodecError> {
        let requirement = match self.kind {
            Access::Whole => try_lower_root_requirement(policy, self.frontier, resources)?,
            Access::Exact => {
                if self.path_unusable {
                    return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                        contract: "codec pushdown prefix contains a non-static step",
                    }));
                }
                let path: Vec<StaticForwardStep<'_>> = self.path.iter().map(PackedStep::as_forward).collect();
                let requirement = try_lower_forward_requirement(policy, &path, resources)?;
                match self.frontier {
                    Some(depth) => requirement.with_lazy_frontier(depth),
                    None => requirement,
                }
            }
        };
        self.finish_requirement(requirement, resources)
    }

    fn charge_non_lenient(
        &self,
        policy: CodecRequirementPolicy,
        resources: &ResourceContext<'_>,
    ) -> Result<AccessRequirement, CodecError> {
        let requirement = try_lower_root_requirement(policy, None, resources)?;
        let mut requirement = requirement;
        self.clauses.attach(&mut requirement)?;
        Ok(requirement)
    }

    fn finish_requirement(
        &self,
        mut requirement: AccessRequirement,
        resources: &ResourceContext<'_>,
    ) -> Result<AccessRequirement, CodecError> {
        if let Some(tree) = &self.prune {
            requirement = requirement.with_prune(try_lower_prune_tree(tree, resources)?);
        }
        self.apply_hints(requirement)
    }

    fn apply_hints(&self, mut requirement: AccessRequirement) -> Result<AccessRequirement, CodecError> {
        if let Some(demand) = &self.count {
            requirement = requirement.with_count(demand.clone());
        }
        if let Some(demand) = &self.element {
            requirement = requirement.with_element(demand.clone());
        }
        if let Some(key) = &self.has_key {
            requirement = requirement.with_has_key(key.clone());
        }
        if self.keys {
            requirement = requirement.with_keys_demand();
        }
        if self.type_demand {
            requirement = requirement.with_type_demand();
        }
        if let Some(fields) = &self.element_construct {
            requirement = requirement.with_element_construct(fields.clone());
        }
        if let Some(hint) = &self.minmax {
            requirement = requirement.with_minmax(hint.clone());
        }
        self.clauses.attach(&mut requirement)?;
        Ok(requirement)
    }

    /// Whole footprint with this job's prune, count, and element. JSON Exact
    /// miss must not re-decode a hint-free document.
    fn charge_whole_rebind(
        &self,
        policy: CodecRequirementPolicy,
        resources: &ResourceContext<'_>,
    ) -> Result<AccessRequirement, CodecError> {
        let mut requirement = try_lower_root_requirement(policy, self.frontier, resources)?;
        let tree = self.rebind_prune.as_ref().or(self.prune.as_ref());
        if let Some(tree) = tree {
            requirement = requirement.with_prune(try_lower_prune_tree(tree, resources)?);
        }
        self.apply_hints(requirement)
    }

    fn charge_range(
        &self,
        policy: CodecRequirementPolicy,
        resources: &ResourceContext<'_>,
    ) -> Result<AccessRequirement, CodecError> {
        let path = self.range_path.as_ref().ok_or_else(|| {
            CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "range-locate requirement for an ineligible program",
            })
        })?;
        let path: Vec<StaticForwardStep<'_>> = path.iter().map(PackedStep::as_forward).collect();
        try_lower_forward_requirement(policy, &path, resources)
    }

    /// JSON Exact count/element miss cannot run the graph on the located node.
    pub(crate) fn may_rebind_whole(&self) -> bool {
        matches!(self.kind, Access::Exact) && (self.count.is_some() || self.element.is_some())
    }

    /// The prune tree `try_requirement` attaches, when the packed plan kept one.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn prune_tree(&self) -> Option<&crate::analysis::PruneNode> {
        self.prune.as_ref()
    }

    /// Packed static prefix as codec path steps. Empty is the whole-document arm.
    pub(crate) fn path_steps(&self) -> Vec<StaticForwardStep<'_>> {
        self.path.iter().map(PackedStep::as_forward).collect()
    }
}

/// Whole [`AccessPlan`] with empty oracle fields. Arms fill the job they pack.
fn whole_plan(
    frontier: Option<u32>,
    prune: Option<crate::analysis::PruneNode>,
    clauses: PackedClauses,
    range_path: Option<Vec<PackedStep>>,
) -> AccessPlan {
    AccessPlan {
        kind: Access::Whole,
        path: Vec::new(),
        path_unusable: false,
        frontier,
        prune,
        rebind_prune: None,
        count: None,
        element: None,
        has_key: None,
        keys: false,
        type_demand: false,
        element_construct: None,
        minmax: None,
        clauses,
        range_path,
    }
}

fn pack_whole(
    shortcut: &Shortcut,
    prune: Option<&crate::analysis::PruneNode>,
    policy: CodecRequirementPolicy,
    consumes_whole: bool,
    clauses: PackedClauses,
    range_path: Option<Vec<PackedStep>>,
) -> AccessPlan {
    match shortcut {
        Shortcut::Count(demand) => AccessPlan {
            count: Some(demand.clone()),
            ..whole_plan(
                Some(consumer_frontier(policy, false)),
                crate::analysis::count::count_prune_tree(demand),
                clauses,
                range_path,
            )
        },
        Shortcut::Element { demand, construct, .. } => {
            let span_fan_out = matches!(demand.row, jqf_data::ElementRow::FanOut);
            AccessPlan {
                element: Some(demand.clone()),
                element_construct: construct.clone(),
                ..whole_plan(
                    Some(consumer_frontier(policy, demand.range.is_some() || span_fan_out)),
                    prune.cloned(),
                    clauses,
                    range_path,
                )
            }
        }
        Shortcut::Type(path) if path.is_empty() => {
            let frontier = Some(consumer_frontier(policy, false));
            AccessPlan {
                type_demand: true,
                ..whole_plan(
                    frontier,
                    if frontier == Some(0) { prune.cloned() } else { None },
                    clauses,
                    range_path,
                )
            }
        }
        _ => {
            let frontier = match (policy.lazy_frontier(), prune.is_some()) {
                (Some(depth), _) => Some(depth),
                (None, true) => Some(0),
                (None, false) => {
                    if consumes_whole {
                        Some(0)
                    } else {
                        None
                    }
                }
            };
            whole_plan(
                frontier,
                if frontier == Some(0) { prune.cloned() } else { None },
                clauses,
                range_path,
            )
        }
    }
}

/// Exact [`AccessPlan`] with empty oracle fields. Arms fill the job they pack.
fn exact_plan(
    kind: Access,
    path: Vec<PackedStep>,
    clauses: PackedClauses,
    range_path: Option<Vec<PackedStep>>,
) -> AccessPlan {
    AccessPlan {
        kind,
        path,
        path_unusable: false,
        frontier: None,
        prune: None,
        rebind_prune: None,
        count: None,
        element: None,
        has_key: None,
        keys: false,
        type_demand: false,
        element_construct: None,
        minmax: None,
        clauses,
        range_path,
    }
}

fn pack_exact(
    program: &Program,
    shortcut: &Shortcut,
    access: Access,
    prune: Option<&crate::analysis::PruneNode>,
    policy: CodecRequirementPolicy,
    clauses: PackedClauses,
    range_path: Option<Vec<PackedStep>>,
) -> AccessPlan {
    match shortcut {
        Shortcut::Count(demand) => {
            let path = pack_count_path(&demand.path);
            AccessPlan {
                frontier: Some(consumer_frontier(policy, false)),
                prune: count_prune_at_container(demand),
                rebind_prune: crate::analysis::count::count_prune_tree(demand),
                count: Some(demand.clone()),
                ..exact_plan(access, path, clauses, range_path)
            }
        }
        Shortcut::Element { demand, construct, .. } => {
            let span_fan_out = matches!(demand.row, jqf_data::ElementRow::FanOut);
            let path = pack_count_path(&demand.path);
            // Locate the container; Each is the iteration *at* that node, so walk
            // the analysis tree along `demand.path` even when the residual starts
            // at `.[]`. YAML Exact prune is the located subtree.
            let at_container = walk_prune(prune, &path).cloned();
            AccessPlan {
                frontier: Some(consumer_frontier(policy, demand.range.is_some() || span_fan_out)),
                prune: at_container,
                rebind_prune: prune.cloned(),
                element: Some(demand.clone()),
                element_construct: construct.clone(),
                ..exact_plan(access, path, clauses, range_path)
            }
        }
        Shortcut::Has(demand) => {
            let path = pack_count_path(&demand.path);
            AccessPlan {
                prune: reanchor_prune(program, prune, &path).cloned(),
                has_key: Some(demand.key.clone()),
                ..exact_plan(access, path, clauses, range_path)
            }
        }
        Shortcut::Keys(path) => {
            let packed = pack_count_path(path);
            AccessPlan {
                prune: reanchor_prune(program, prune, &packed).cloned(),
                keys: true,
                ..exact_plan(access, packed, clauses, range_path)
            }
        }
        Shortcut::AnyAll(demand) => {
            let count = jqf_data::CountDemand {
                row: jqf_data::CountRow::Collect,
                path: demand.path.clone(),
                range: None,
                probe: Vec::new(),
                filter: Some(demand.filter.clone()),
            };
            let path = pack_count_path(&demand.path);
            AccessPlan {
                frontier: Some(consumer_frontier(policy, false)),
                prune: count_prune_at_container(&count),
                rebind_prune: crate::analysis::count::count_prune_tree(&count),
                count: Some(count),
                ..exact_plan(access, path, clauses, range_path)
            }
        }
        Shortcut::MinMax(demand) => {
            let path = pack_count_path(&demand.path);
            AccessPlan {
                prune: reanchor_prune(program, prune, &path).cloned(),
                minmax: Some(jqf_data::MinMaxHint {
                    op: demand.op,
                    probe: demand.probe.clone(),
                }),
                ..exact_plan(access, path, clauses, range_path)
            }
        }
        _ => match pack_pushdown(program) {
            Ok(path) => AccessPlan {
                prune: reanchor_prune(program, prune, &path).cloned(),
                type_demand: matches!(shortcut, Shortcut::Type(_)),
                ..exact_plan(access, path, clauses, range_path)
            },
            Err(()) => AccessPlan {
                path_unusable: true,
                type_demand: matches!(shortcut, Shortcut::Type(_)),
                ..exact_plan(access, Vec::new(), clauses, range_path)
            },
        },
    }
}

impl CompiledProgram {
    /// Instantiates the packed access plan into a codec requirement on this
    /// request's resource ledger. Planning already happened at finish; this
    /// reserves path, prune, and hints and can fail if the ledger refuses.
    ///
    /// A non-lenient mismatch policy still rebinds Whole so engine cells fire:
    /// the codec's pushed-down prefix would answer missing/null without reaching
    /// those cells. The document may still be lazy.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when charging the packed plan against the request
    /// ledger fails.
    pub fn try_requirement(&self, resources: &ResourceContext<'_>) -> Result<AccessRequirement, CodecError> {
        if resources.mismatch_policy() != MismatchPolicy::Lenient {
            return self.plan.charge_non_lenient(self.policy, resources);
        }
        self.plan.charge(self.policy, resources)
    }

    /// Whole-document charge of the packed job for [`crate::exec::EngineRun::ReboundWhole`].
    ///
    /// Footprint is Whole; prune, count, and element stay the job's. Exact
    /// count/element store a container-rooted prune for the first bind; this
    /// charge uses the document-rooted tree so `.users` is kept.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when charging against the request ledger fails.
    pub fn try_rebind_whole_requirement(
        &self,
        resources: &ResourceContext<'_>,
    ) -> Result<AccessRequirement, CodecError> {
        self.plan.charge_whole_rebind(self.policy, resources)
    }

    /// Lowers a WHOLE-DOCUMENT requirement under this program's policy,
    /// whatever the program's own path class is.
    ///
    /// The EAGER drives — `-n`, `-s`, and the record slurp sibling — materialize
    /// every input value into an owned value before the single run, so each
    /// per-value decode must produce a complete document. Lowering the
    /// PROGRAM's own requirement there binds a scoped route per input value
    /// whose located outcome the drive cannot interpret (on the merge base,
    /// `jqf -s '.[0]'` failed with "input-sequence decode produced a
    /// non-result outcome", exit 5, where the answer is the first value).
    /// The record `-n` drive decodes each pulled record on demand under
    /// [`Self::try_pulled_record_requirement`] instead.
    ///
    /// The frontier is explicitly 0 (eager) because the drive materializes the
    /// value in full: deferring container spans would only pay to re-enter
    /// them (a deferred span is re-parsed on materialization). This overrides
    /// the LAZY default that `try_lower_root_requirement` applies to `None`;
    /// the doc there is the one authority for the default.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when charging the lowered requirement against
    /// the request ledger fails.
    pub fn try_whole_document_requirement(
        &self,
        resources: &ResourceContext<'_>,
    ) -> Result<AccessRequirement, CodecError> {
        try_lower_root_requirement(self.policy, Some(0), resources)
    }

    /// Whole-document requirement for a pulled record, with the pulled-record
    /// prune hint when the walk bounded the pull below the whole record.
    /// Eager (frontier 0): the hint and deferral do not compose. The record
    /// `-n` drive binds this per pull.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when lowering the requirement or the prune
    /// transport fails.
    pub fn try_pulled_record_requirement(
        &self,
        resources: &ResourceContext<'_>,
    ) -> Result<AccessRequirement, CodecError> {
        let requirement = try_lower_root_requirement(self.policy, Some(0), resources)?;
        match &self.pulled_record {
            Some(tree) => Ok(requirement.with_prune(try_lower_prune_tree(tree, resources)?)),
            None => Ok(requirement),
        }
    }

    /// The packed range-locate path as an exact codec located requirement.
    ///
    /// The footprint and schedule are byte-identical to the range-COUNT
    /// lowering of the same path; only the result authority differs, so the
    /// bare-slice publish shares the range row's validate/locate/resolve work
    /// exactly as the P0 rung shares the scoped route's.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when called on an ineligible program or when
    /// charging the packed path against the request ledger fails.
    pub fn try_range_locate_requirement(
        &self,
        resources: &ResourceContext<'_>,
    ) -> Result<AccessRequirement, CodecError> {
        self.plan.charge_range(self.policy, resources)
    }

    /// Test helper: the named keys of the re-anchored prune, or `None` when
    /// re-anchoring declines (residual starts at `.[]`, or the tree cannot
    /// be rooted at the pushed-down node).
    #[cfg(test)]
    pub(crate) fn reanchor_prune_keys(&self) -> Option<Vec<String>> {
        let (tree, _) = crate::analysis::prune_trees(self.program.nodes(), self.program.root(), self.program.slots());
        let path = pack_pushdown(&self.program).ok()?;
        let node = reanchor_prune(&self.program, tree.as_ref(), &path)?;
        Some(node.keys().keys().cloned().collect())
    }

    /// The pushed-down static prefix as codec path steps, for the explain plan.
    ///
    /// Reads the path finish stored on the access plan. Empty is the
    /// whole-document arm.
    #[must_use]
    pub fn pushdown_path(&self) -> Vec<StaticForwardStep<'_>> {
        self.plan.path_steps()
    }
}
