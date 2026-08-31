//! The opaque compiled program and the accessors drives read.
//!
//! Owns [`CompiledProgram`], its codec-requirement lowering, and the residual
//! run entries. Route-class facts (count, element, keys, type, prune) derive
//! once in [`CompiledProgram::finish`] and are read from cache thereafter.

#[allow(clippy::wildcard_imports)]
use super::*;

#[cfg(test)]
extern crate std;

/// How many times the recognized count demand has been DERIVED (the arena
/// walk plus the projection classification) rather than read from the cached
/// field — test-only: the derivation-cache regression test asserts the
/// per-record consultation pattern never re-derives.
///
/// Every derive takes [`DERIVATION_LOCK`] and that test holds the same lock
/// across its snapshot-to-assertion window, so a concurrent test's compile
/// cannot move the counter mid-assertion: without the lock, the shared
/// counters race every other test that compiles a program and the "no
/// re-derivation" assertion flakes.
#[cfg(test)]
pub(crate) static COUNT_DEMAND_DERIVATIONS: AtomicUsize = AtomicUsize::new(0);

/// How many times the recognized element-iteration demand has been DERIVED
/// (the fan-out/reduce-fold arena walk) rather than read from the cached
/// field — test-only: the derivation-cache regression test's element twin.
#[cfg(test)]
pub(crate) static ELEMENT_DEMAND_DERIVATIONS: AtomicUsize = AtomicUsize::new(0);

/// The test-only lock that makes the derivation counters race-free (see
/// [`COUNT_DEMAND_DERIVATIONS`]). Tests build in the `std` world even though
/// the crate is `no_std`, so the mutex is available here.
#[cfg(test)]
pub(crate) static DERIVATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A compiled program: prelude-gated, lowered, transformed, analyzed, and
/// finished — ready to lower into a codec requirement and interpret run
/// outcomes.
///
/// The program is opaque: callers observe it only through [`try_requirement`]
/// and [`try_run`]. Internally it carries the engine `Program` arena (in
/// path-normal form — a bare `Stage`, or a `Choice`/`FlatMap` graph a comma
/// blocked from fusing), its derived codec-pushdown split, and the per-component
/// optional flags. [`try_requirement`] lowers the split into a codec access
/// requirement (reusing the existing tested lowerings unchanged); [`try_run`]
/// drives the residual graph under the engine's exact-step suppression law.
///
/// [`try_requirement`]: CompiledProgram::try_requirement
/// [`try_run`]: CompiledProgram::try_run
///
/// `Send + Sync`: a morsel worker borrows the parent program. Retained
/// bytes stay on the parent ledger.
#[derive(Debug)]
pub struct CompiledProgram {
    pub(crate) program: Program,
    policy: CodecRequirementPolicy,
    /// The kept-subtree selection over the document root, resolved ONCE here
    /// by the compile-time-fact argument of the keep-subtree walk (the same
    /// static-path analysis that decides which reads bound below the whole
    /// document). Present when the program's reads bound below
    /// the whole document. Whole-document lowerings attach it at the root;
    /// a static forward prefix re-anchors it onto the located node, and
    /// declines (no hint) when that walk would cross an element.
    prune: Option<crate::analysis::PruneNode>,
    /// Whether the program binds the `~inputs` resident. The input-sequence
    /// cursor is scoped to the null-first drive: a cursor over
    /// the shared input sequence collides with the per-element cursor-store
    /// reset, so the CLI rejects every other route before a byte is read.
    uses_inputs_cursor: bool,
    /// The variable slot `$index` lowered to under the SPLIT compile entry
    /// ([`crate::CompileOptions::split_exp`] via [`crate::try_compile_program`]),
    /// seeded per run with the item counter ([`Self::try_run_split`]). `None` for
    /// every ordinary program and for a split expression that never references
    /// `$index`.
    runtime_index_slot: Option<u32>,
    /// The recognized COUNT demand of this program, derived
    /// ONCE here at construction by the same compile-time-fact argument as
    /// [`Self::prune`]: the per-item drive consults it on every record
    /// ([`Self::count_demand`]), and a per-record re-derivation would charge
    /// the arena walk plus the projection classification to every streamed
    /// value for a fact that cannot change between them.
    count: Option<jqf_data::CountDemand>,
    /// The recognized ELEMENT-ITERATION demand, derived ONCE
    /// here at construction for the same reason as [`Self::count`] — the
    /// per-item drive consults it beside the count demand on every record.
    element: Option<jqf_data::ElementDemand>,
    /// Static-key object construction riding an element-iteration fan-out
    /// (`PATH[] | {static keys}`). `None` for every other row. The SDK
    /// reconstructs each published object from these fields; the demand's
    /// probe is the empty path (the whole element).
    element_construct: Option<crate::analysis::element::ConstructFields>,
    /// The program is `[FAN-OUT]`: the SDK collects every visited probe
    /// value into one array and publishes that array as the sole item.
    element_collect: bool,
    /// The recognized KEYS-publish path (`keys` / `PATH | keys`). Empty is
    /// the document root. `None` when the program is not a keys row.
    keys: Option<alloc::vec::Vec<jqf_data::CountStep>>,
    /// The static path a type-class program names (`type` / `PATH | type`).
    /// Empty is the document root. `None` when the program is not a type row.
    type_path: Option<alloc::vec::Vec<jqf_data::CountStep>>,
    /// Kept-subtree selection over a pulled `input`/`inputs` record.
    /// Present when the program pulls and the pulled demand bounds below
    /// the whole record. The record `NullFirst` drive attaches it as a
    /// per-record hint; the root prune tree stays declined.
    pulled_record: Option<crate::analysis::PruneNode>,
}

struct RouteDemands {
    count: Option<jqf_data::CountDemand>,
    element: Option<jqf_data::ElementDemand>,
    element_construct: Option<crate::analysis::element::ConstructFields>,
    element_collect: bool,
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    // Workers borrow the parent program. A
    // request-local account inside this type would put that account on a
    // worker thread; do not weaken detach to paper over it.
    assert_send_sync::<CompiledProgram>();
};

fn insert_clause(requirement: &mut AccessRequirement, clause: &DemandClause) -> Result<(), CodecError> {
    requirement.try_insert_clause(clause).map_err(CodecError::from)
}

impl CompiledProgram {
    /// The kept-subtree selection over the document root, when the program's
    /// reads bound below the whole document. Test-only for now; the
    /// `--explain` surface will want a rendered form when the route
    /// diagnostics land.
    #[cfg(test)]
    pub(crate) fn prune_tree(&self) -> Option<&crate::analysis::PruneNode> {
        self.prune.as_ref()
    }

    /// The kept-subtree selection over a pulled `input`/`inputs` record.
    #[cfg(test)]
    pub(crate) fn pulled_record_prune(&self) -> Option<&crate::analysis::PruneNode> {
        self.pulled_record.as_ref()
    }

    /// Whether the program binds the `~inputs` resident — the null-first-drive
    /// scoping flag the CLI's route planner reads.
    #[must_use]
    pub fn uses_inputs_cursor(&self) -> bool {
        self.uses_inputs_cursor
    }

    /// Whether the live graph reachable from the root contains a `FlatMap`.
    ///
    /// See [`crate::program::Program::has_reachable_flatmap`].
    #[must_use]
    pub fn has_reachable_flatmap(&self) -> bool {
        self.program.has_reachable_flatmap()
    }

    /// The split-expansion `$index` slot recorded at compile time, when present.
    #[must_use]
    pub fn runtime_index_slot(&self) -> Option<u32> {
        self.runtime_index_slot
    }

    /// Lowers the recognized program into an exact codec access requirement.
    ///
    /// The derived split selects the lowering: identity reuses
    /// [`try_lower_root_requirement`] (whole-document access), and a static
    /// forward path reuses [`try_lower_forward_requirement`] with the decoded
    /// steps. Identity never lowers as a forward requirement with an empty path,
    /// preserving the whole-document-versus-exact-root distinction.
    pub fn try_requirement(&self, resources: &ResourceContext<'_>) -> Result<AccessRequirement, CodecError> {
        // The mismatch dial beyond lenient: the codec's
        // pushed-down prefix navigation answers missing/null without ever
        // reaching the engine's cell sites, so under warn/strict the whole
        // document is resolved and the ENGINE walks every step — its cells
        // are the reference evaluation's cells. This is the route-decline law
        // applied at the requirement level; a non-lenient request never lets
        // a decode resolve a prefix it cannot report on. The document may
        // still be LAZY (the default): the engine's walk materializes
        // exactly the subtrees it touches and fires its own cells over them,
        // so the decline law — about the codec never answering missing/null —
        // is unchanged.
        if resources.mismatch_policy() != MismatchPolicy::Lenient {
            return self.attach_program_demand(try_lower_root_requirement(self.policy, None, resources)?);
        }
        // A count-class program lowers the WHOLE-DOCUMENT requirement plus the
        // Count demand hint and a kept-subtree prune (the element COUNT is the
        // only observable, so no payload is owed). Unbounded count visits every
        // element boundary, so the default is EAGER: that arms the prune on
        // JSON (which otherwise drops it under a nonzero frontier and re-walks
        // the fat array's span). A bounded element prefix stays lazy — see
        // [`Self::consumer_frontier`]. Policy override still wins. Consumer
        // decline runs the whole program over the outcome.
        if let Some(demand) = self.count_demand() {
            let frontier = self.consumer_frontier(false);
            let requirement = try_lower_root_requirement(self.policy, frontier, resources)?;
            let requirement = match crate::analysis::count::count_prune_tree(demand) {
                Some(tree) => requirement.with_prune(try_lower_prune_tree(&tree, resources)?),
                None => requirement,
            };
            return self.attach_program_demand(requirement.with_count(demand.clone()));
        }
        // An element-iteration program lowers the WHOLE-DOCUMENT requirement
        // with an ELEMENT demand hint and the program's kept-subtree prune.
        // Fan-out stays lazy so JSON keeps the container as a span; the span
        // leaf extracts the probe (and a select filter) without building every
        // element. Reduce-fold without a range defaults eager so prune-armed
        // JSON can skip unread members of fat fold elements. `limit`/`first`/
        // `nth` keep a bounded `demand.range` and stay lazy. Policy override
        // still wins.
        if let Some(demand) = self.element_demand() {
            let span_fan_out = matches!(demand.row, jqf_data::ElementRow::FanOut);
            let frontier = self.consumer_frontier(demand.range.is_some() || span_fan_out);
            let requirement = try_lower_root_requirement(self.policy, frontier, resources)?;
            let requirement = match &self.prune {
                Some(tree) => requirement.with_prune(try_lower_prune_tree(tree, resources)?),
                None => requirement,
            };
            return self.attach_program_demand(requirement.with_element(demand.clone()));
        }
        // Bare `type` is a kind-only read of the document root: Whole plus
        // the type hint so a codec can skip payload (XML measure-skeleton).
        // `PATH | type` is not this arm — the executor still consumes the
        // pushdown prefix, so Whole would type the root. That row Exact-locates
        // PATH and the residual `type` reads the located node; the hint rides
        // the Exact requirement below. `type?` is not a row.
        if self.type_demand_path().is_some_and(<[_]>::is_empty) {
            let frontier = self.consumer_frontier(false);
            let requirement = try_lower_root_requirement(self.policy, frontier, resources)?;
            let requirement = match &self.prune {
                Some(tree) if frontier == Some(0) => requirement.with_prune(try_lower_prune_tree(tree, resources)?),
                _ => requirement,
            };
            return self.attach_program_demand(requirement.with_type_demand());
        }
        if self.program.split().is_whole_document() {
            // The route decision travels WITH the requirement: the policy's
            // explicit override wins (the force-lazy differential's env var),
            // otherwise a prune tree forces EAGER — deferral would only
            // re-enter subtrees the tree already bounds — and everything else
            // takes the LAZY default, decided in ONE place inside
            // `try_lower_root_requirement` (whole-document consumers included).
            // The whole-document route (slot 0) reads the
            // frontier at open, so library callers get the lazy default with
            // no process global involved.
            //
            // A prune tree rides the requirement as a monotone hint, and only
            // when the EFFECTIVE frontier is eager (`Some(0)`): deferral and
            // the hint never compose, and a nonzero frontier drops the hint
            // rather than mix with the deferral machinery (the hint is
            // optional by contract).
            let frontier = match (self.policy.lazy_frontier(), self.prune.is_some()) {
                (Some(depth), _) => Some(depth),
                (None, true) => Some(0),
                // Consumption-class routing: a program that PROVABLY
                // materializes the whole
                // document anyway — identity, `..`, whole-document folds —
                // decodes EAGER. Its `consumes_whole_document` classification
                // is the full-materialization signal: the lazy skeleton would
                // only be re-parsed on materialization and retained beside
                // the materialized tree (the deep-collect/identity RSS debt),
                // so the core never builds it. A partial reader (`.a // .b`, a
                // `try`/`catch`) stays on the lazy default and materializes
                // exactly the subtrees it touches.
                (None, false) => {
                    if self.consumes_whole_document() {
                        Some(0)
                    } else {
                        None
                    }
                }
            };
            let requirement = try_lower_root_requirement(self.policy, frontier, resources)?;
            return match (&self.prune, frontier) {
                (Some(tree), Some(0)) => {
                    self.attach_program_demand(requirement.with_prune(try_lower_prune_tree(tree, resources)?))
                }
                _ => self.attach_program_demand(requirement),
            };
        }
        let path = self.pushdown_prefix_path()?;
        let requirement = try_lower_forward_requirement(self.policy, &path, resources)?;
        // Re-anchor the root-anchored kept-subtree tree onto the located
        // node. The walk follows only named object keys: an array index or a
        // residual that starts at `.[]` would prune across an element, and
        // the hint would no longer be rooted at the pushed-down node. Fail
        // closed — over-delivery is always sound.
        let requirement = if self.type_demand() {
            requirement.with_type_demand()
        } else {
            requirement
        };
        match self.reanchor_prune(&path) {
            Some(tree) => self.attach_program_demand(requirement.with_prune(try_lower_prune_tree(tree, resources)?)),
            None => self.attach_program_demand(requirement),
        }
    }

    /// Inserts demand clauses the program's accessors name: `.@tag` as
    /// [`DemandClause::IntrinsicTag`], other `.@role` as
    /// [`DemandClause::AttachedFact`], and `.&name` as
    /// [`DemandClause::Attribute`]. Dynamic `.@(expr)` / `.&(expr)` cannot
    /// name the identity, so they keep every catalogued fact and the tag.
    fn attach_program_demand(&self, mut requirement: AccessRequirement) -> Result<AccessRequirement, CodecError> {
        let mut want_tag = false;
        let mut want_all_facts = false;
        let mut want_topology = false;
        let mut facts = Vec::new();
        let mut attributes = Vec::new();
        for node in self.program.nodes() {
            if let ProgramNode::Call { overload, .. } = node
                && (overload.get() == jqf_builtins::registry::builtins::id::XPATH_1
                    || overload.get() == jqf_builtins::registry::builtins::id::CSS_1)
            {
                // xpath/css read name, attrs, textContent, and child/owner
                // topology. The engine cannot parse the selector string here;
                // catalogued markup facts plus topology are the closed
                // over-approximation.
                want_all_facts = true;
                want_topology = true;
            }
            let ProgramNode::Stage { steps, .. } = node else {
                continue;
            };
            for step in steps {
                match step.access() {
                    StepAccess::NodeAccessor(name) if name == "tag" => want_tag = true,
                    StepAccess::NodeAccessor(name) => facts.push(name.as_str()),
                    StepAccess::DynNodeAccessor(_) => {
                        want_tag = true;
                        want_all_facts = true;
                    }
                    StepAccess::Attribute(name) => attributes.push(name.as_str()),
                    StepAccess::DynAttribute(_) => want_all_facts = true,
                    _ => {}
                }
            }
        }
        if want_tag {
            insert_clause(&mut requirement, &DemandClause::IntrinsicTag)?;
        }
        if want_all_facts {
            for role in jqf_codec_core::ATTACHED_FACT_ROLES {
                insert_clause(
                    &mut requirement,
                    &DemandClause::try_attached_fact(role).ok_or_else(|| {
                        CodecError::new(CodecFailureKind::InternalContractViolation {
                            contract: "attached-fact demand identity",
                        })
                    })?,
                )?;
            }
        } else {
            for role in facts {
                if !jqf_codec_core::ATTACHED_FACT_ROLES.contains(&role) {
                    continue;
                }
                insert_clause(
                    &mut requirement,
                    &DemandClause::try_attached_fact(role).ok_or_else(|| {
                        CodecError::new(CodecFailureKind::InternalContractViolation {
                            contract: "attached-fact demand identity",
                        })
                    })?,
                )?;
            }
        }
        for name in attributes {
            let expanded = ExpandedName::try_new("", name).map_err(|_| {
                CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "attribute demand identity",
                })
            })?;
            insert_clause(&mut requirement, &DemandClause::Attribute(expanded))?;
        }
        if want_topology {
            insert_clause(
                &mut requirement,
                &DemandClause::Topology(jqf_codec_core::TopologyDemand::Children),
            )?;
            insert_clause(
                &mut requirement,
                &DemandClause::Topology(jqf_codec_core::TopologyDemand::Owner),
            )?;
        }
        Ok(requirement)
    }

    /// Runs this program over one SYNTHESIZED value with no codec pushdown:
    /// every static step the split handed to the codec is applied by the
    /// engine instead.
    ///
    /// [`Self::try_run`] assumes the codec already resolved the pushed-down
    /// prefix, which is true for every drive whose input came OUT of a decode.
    /// The single-run drives (`-n`, `-s`, and their record siblings) build
    /// their input themselves — `null`, or the array of every decoded input —
    /// so nothing resolved the prefix and the whole program must run
    /// (`-s '.[0]'` answers the first value, and running the residual alone
    /// answers the whole array).
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when seeding the executor fails.
    pub fn try_run_whole_value<'source>(
        &self,
        outcome: CodecInputOutcome<'source>,
        _resources: &ResourceContext<'_>,
    ) -> Result<EngineRun<'_, 'source>, CodecError> {
        try_run(
            self.program.nodes(),
            self.program.root(),
            self.program.split().entry(),
            0,
            self.program.entry_stage_steps(),
            self.program.slots(),
            self.program.scans(),
            self.program.topk_sorts(),
            self.program.anti_joins(),
            outcome,
            None,
        )
    }

    /// Runs this program over one value with `$index` BOUND to the item
    /// counter — the split-destination run. Whole
    /// semantics exactly like [`Self::try_run_whole_value`] (nothing resolved
    /// the split program's prefix; the whole program runs), with the
    /// difference that `$index`'s compile-time slot is seeded with `index`
    /// before the first poll.
    ///
    /// The item counter binds as an exact integer. The seed is a no-op when
    /// the expression never references `$index` (the slot is `None`).
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when seeding the executor fails.
    pub fn try_run_split<'source>(
        &self,
        outcome: CodecInputOutcome<'source>,
        index: u64,
        _resources: &ResourceContext<'_>,
    ) -> Result<EngineRun<'_, 'source>, CodecError> {
        let runtime_index = self.runtime_index_slot.map(|slot| {
            (
                slot,
                jqf_data::Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(
                    i64::try_from(index).unwrap_or(i64::MAX),
                ))),
            )
        });
        try_run(
            self.program.nodes(),
            self.program.root(),
            self.program.split().entry(),
            0,
            self.program.entry_stage_steps(),
            self.program.slots(),
            self.program.scans(),
            self.program.topk_sorts(),
            self.program.anti_joins(),
            outcome,
            runtime_index,
        )
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

    /// Whether the compiled graph calls any input-family builtin (`input`,
    /// `inputs`, `input_filename`, `input_line_number`).
    ///
    /// The host drive must satisfy those calls with a shared input sequence:
    /// the CLI routes a program that needs one to the input-sequence drive
    /// instead of the ordinary per-value sequence, so a `try input catch .`
    /// program and a plain `.a` program never share a decode regime.
    #[must_use]
    pub fn uses_input_family(&self) -> bool {
        self.program.nodes().iter().any(|node| match node {
            ProgramNode::Call { overload, .. } => matches!(
                overload.get(),
                jqf_builtins::registry::builtins::id::INPUT
                    | jqf_builtins::registry::builtins::id::INPUTS
                    | jqf_builtins::registry::builtins::id::INPUT_FILENAME
                    | jqf_builtins::registry::builtins::id::INPUT_LINE_NUMBER
            ),
            _ => false,
        })
    }

    /// Recognizes the slurp shape `map(F) | add` / `map(F) | length` on the
    /// compiled arena and returns the equivalent STREAMING fold
    /// (`reduce inputs as $x (INIT; . + ([($x | F)] | AGG))`), built directly
    /// in the arena.
    ///
    /// This was the CLI optimizer's source-text rewrite: `-s 'map(F) | add'`
    /// collected every record into the slurp array first — O(records) of
    /// allocator commit — where the streamed equivalent folds one record at a
    /// time through the `inputs` cursor and is byte-identical (`map` is
    /// `[.[] | F]`; an empty F contribution adds 0). The recognition and the
    /// rewrite now live here, where the program is already compiled, so the
    /// CLI no longer parses and rewrites the user's source text.
    ///
    /// On the arena `map` lowers to `[.[] | F]`, so the shape is a `FlatMap`
    /// whose upstream is a `CollectArray` over the `.[] | F` body and whose
    /// body is an arity-0 `add`/`length` call. The map body is reused with
    /// its leading `.[]` step consumed and the entry re-seated on the fold's
    /// `$x` slot, so the per-record contribution `[($x | F)]` is the same
    /// graph the collect would have run per element. Returns `None` for every
    /// other program.
    #[expect(
        clippy::too_many_lines,
        reason = "one linear arena build: recognize, transform the map body, assemble the fold; splitting it would thread the borrowed arena through helpers that each read one piece"
    )]
    #[must_use]
    pub fn streaming_slurp_aggregate(&self) -> Option<CompiledProgram> {
        let nodes = self.program.nodes();
        let root = self.program.root();
        let (map_body, suffix) = match &nodes[root.index()] {
            ProgramNode::FlatMap { upstream, body } => {
                let ProgramNode::Call { overload, args, .. } = &nodes[body.index()] else {
                    return None;
                };
                if !args.is_empty()
                    || (overload.get() != jqf_builtins::registry::builtins::id::ADD
                        && overload.get() != jqf_builtins::registry::builtins::id::LENGTH)
                {
                    return None;
                }
                let ProgramNode::CollectArray {
                    body: Some(collect_body),
                } = &nodes[upstream.index()]
                else {
                    return None;
                };
                (*collect_body, Some(*body))
            }
            // `[STREAM] | add` is rewritten to a reduce at compile; slurp still
            // needs the map-body `.[]` reseated onto the inputs `$x`.
            ProgramNode::Reduce { source, .. } => (*source, None),
            _ => return None,
        };
        // The collect body must be the `.[] | F` map shape: a fused stage
        // whose leading step is a plain `.[]`, or a `FlatMap` over exactly
        // that stage. The rewrite consumes the `.[]` and re-seats the entry
        // on the fold's `$x` slot.
        let slot = self.program.slots();
        let mut new_nodes = nodes.to_vec();
        let f_graph = match &nodes[map_body.index()] {
            ProgramNode::Stage {
                start: StageStart::Current,
                steps,
            } => {
                let [first, rest @ ..] = steps.as_slice() else {
                    return None;
                };
                if !matches!(first.access(), StepAccess::Each) || first.cell_suppressed() {
                    return None;
                }
                push_rewrite_node(
                    &mut new_nodes,
                    ProgramNode::Stage {
                        start: StageStart::Variable(slot),
                        steps: rest.to_vec(),
                    },
                )?
            }
            ProgramNode::FlatMap { upstream, body } => {
                let ProgramNode::Stage {
                    start: StageStart::Current,
                    steps,
                } = &nodes[upstream.index()]
                else {
                    return None;
                };
                let [first] = steps.as_slice() else {
                    return None;
                };
                if !matches!(first.access(), StepAccess::Each) || first.cell_suppressed() {
                    return None;
                }
                let x_stage = push_rewrite_node(
                    &mut new_nodes,
                    ProgramNode::Stage {
                        start: StageStart::Variable(slot),
                        steps: Vec::new(),
                    },
                )?;
                push_rewrite_node(
                    &mut new_nodes,
                    ProgramNode::FlatMap {
                        upstream: x_stage,
                        body: *body,
                    },
                )?
            }
            _ => return None,
        };
        let is_length = match suffix {
            Some(suffix) => match &nodes[suffix.index()] {
                ProgramNode::Call { overload, .. } => overload.get() == jqf_builtins::registry::builtins::id::LENGTH,
                _ => return None,
            },
            None => false,
        };
        let collect = push_rewrite_node(&mut new_nodes, ProgramNode::CollectArray { body: Some(f_graph) })?;
        let current = push_rewrite_node(
            &mut new_nodes,
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: Vec::new(),
            },
        )?;
        // The suffix call (add/length) is the ROOT's own call node, reused
        // with its pinned overload id and revision. A compile-time
        // `[STREAM]|add` reduce has no suffix call: the per-record
        // contribution is `f_graph` itself.
        let suffix_flat = match suffix {
            Some(suffix) => push_rewrite_node(
                &mut new_nodes,
                ProgramNode::FlatMap {
                    upstream: collect,
                    body: suffix,
                },
            )?,
            None => f_graph,
        };
        let update = push_rewrite_node(
            &mut new_nodes,
            ProgramNode::Binary {
                op: BinaryKind::Add,
                left: current,
                right: suffix_flat,
                shape: crate::program::BinaryShape::Framed,
            },
        )?;
        // `inputs` is the record cursor of the null-first drive the caller
        // runs the fold under, exactly as the CLI's rewritten source named it.
        let inputs = resolve_builtin("inputs", 0)?;
        let inputs_call = push_rewrite_node(
            &mut new_nodes,
            ProgramNode::call(inputs.id, inputs.semantic_revision, Vec::new()),
        )?;
        // `null` is the `+` identity for both numbers and strings (the add
        // aggregate's init); the length aggregate's init is the exact integer
        // zero.
        let init_value = if is_length {
            Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(0)))
        } else {
            Value::Null
        };
        let init = push_rewrite_node(
            &mut new_nodes,
            ProgramNode::Stage {
                start: StageStart::Literal(init_value),
                steps: Vec::new(),
            },
        )?;
        let reduce = push_rewrite_node(
            &mut new_nodes,
            ProgramNode::Reduce {
                source: inputs_call,
                slot,
                init,
                update,
                keyed_collect: None,
            },
        )?;
        crate::exec::GraphMachine::mark_keyed_collects(&mut new_nodes);
        let split = analyze(&new_nodes, reduce);
        let program = Program::new(new_nodes, reduce, split, slot + 1);
        // The rewrite's own epilogue differences live in `finish`'s doc: the
        // prune gate this path used to carry was a no-op (the fold's
        // `inputs` source pull declines the root tree inside the analysis),
        // and the marks stay here because these nodes are hand-built with
        // explicit shape fields.
        Some(CompiledProgram::finish(program, self.policy, false, None))
    }

    /// The pushed-down prefix as codec path steps.
    ///
    /// Only the entry stage's pre-first-`Each` prefix is pushed to the codec; the
    /// residual (from the first `.[]` on, plus the whole downstream graph) is the
    /// executor's. The prefix therefore contains no `Each` step by construction.
    /// The entry stage may sit inside a `FlatMap` upstream (`.a[] | (.b, .c)`),
    /// so this reads the entry stage, not the root. An empty prefix yields the
    /// empty path — the exact ROOT selection, which the whole-document arm of
    /// [`Self::try_requirement`] deliberately does not use but the forward
    /// requirement lowering ([`Self::try_requirement`]'s located arm, feeding
    /// `try_lower_forward_requirement`) does.
    fn pushdown_prefix_path(&self) -> Result<Vec<StaticForwardStep<'_>>, CodecError> {
        let steps = self.program.entry_stage_steps();
        let prefix = &steps[..self.program.split().prefix_len()];
        let mut path = Vec::new();
        path.try_reserve_exact(prefix.len())
            .map_err(|_| CodecError::new(CodecFailureKind::AllocationFailure))?;
        for step in prefix {
            // Optionality is interpretation, never access: the requirement reads
            // only the step's access, so `.a? | .b` and `.a | .b` lower to the
            // identical requirement.
            path.push(match step.access() {
                StepAccess::Key(key) => StaticForwardStep::ObjectKey(key.as_str()),
                StepAccess::Index(index) => StaticForwardStep::ArrayIndex(*index),
                StepAccess::Each
                | StepAccess::DynVar(_)
                | StepAccess::Descend
                | StepAccess::Slice(_)
                | StepAccess::NodeAccessor(_)
                | StepAccess::Attribute(_)
                | StepAccess::DynNodeAccessor(_)
                | StepAccess::DynAttribute(_) => {
                    return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                        contract: "codec pushdown prefix contains a non-static step",
                    }));
                }
            });
        }
        Ok(path)
    }

    /// Whether the residual begins at a `.[]` the prefix stopped before.
    ///
    /// The codec path never contains `Each` (the prefix is truncated there),
    /// so this is the only way the pushed-down node sits *above* an element
    /// the program then iterates. Re-anchoring through that boundary would
    /// prune across `.[]`.
    fn residual_starts_at_each(&self) -> bool {
        let steps = self.program.entry_stage_steps();
        matches!(
            steps.get(self.program.split().prefix_len()).map(StageStep::access),
            Some(StepAccess::Each)
        )
    }

    /// Walks `tree` along `path` and returns the subtree rooted at the
    /// located node, or `None` when the walk cannot prove that subtree is
    /// still the prune of the pushed-down node.
    fn reanchor_prune<'a>(&'a self, path: &[StaticForwardStep<'_>]) -> Option<&'a crate::analysis::PruneNode> {
        let mut node = self.prune.as_ref()?;
        if self.residual_starts_at_each() {
            return None;
        }
        for step in path {
            if node.is_all() {
                // The located subtree is already kept whole; a keep-all hint
                // is a no-op on Exact/Located.
                return None;
            }
            match step {
                StaticForwardStep::ObjectKey(key) => {
                    // Named-key only: falling through to the shared element
                    // child would prune across an array (or object-member)
                    // iteration the path never named.
                    node = node.keys().get(*key)?;
                }
                StaticForwardStep::ArrayIndex(_) | StaticForwardStep::ArrayRange { .. } => {
                    return None;
                }
            }
        }
        if node.is_all() {
            return None;
        }
        Some(node)
    }

    /// Test helper: the named keys of the re-anchored prune, or `None` when
    /// re-anchoring declines (path crosses an element, or the tree cannot
    /// be rooted at the pushed-down node).
    #[cfg(test)]
    pub(crate) fn reanchor_prune_keys(&self) -> Option<Vec<String>> {
        let path = self.pushdown_prefix_path().ok()?;
        let node = self.reanchor_prune(&path)?;
        Some(node.keys().keys().cloned().collect())
    }

    /// The pushed-down static prefix as codec path steps, for the explain plan.
    ///
    /// This is the same path the requirement lowerings compute
    /// ([`Self::pushdown_prefix_path`]), exposed infallibly for `--explain`'s
    /// plan renderer: an empty path is the root selection (the whole-document
    /// arm). The prefix contains no `Each` step by construction, so the
    /// lowering's only error arms are unreachable; a renderer that ever saw one
    /// would rather show the empty path than fail the request.
    #[must_use]
    pub fn pushdown_path(&self) -> Vec<StaticForwardStep<'_>> {
        self.pushdown_prefix_path().unwrap_or_default()
    }

    /// The explain plan: this program's routing facts in one borrowable view.
    ///
    /// See [`crate::explain`] — the plan reads every fact through the same
    /// accessor the route selector reads, and changes no routing.
    #[must_use]
    pub fn explain(&self) -> crate::explain::ExplainPlan<'_> {
        crate::explain::plan(self)
    }

    /// Whether this program belongs to the MORSEL STATIC PATH class — a bare
    /// static Key/Index chain over the input, identity included.
    ///
    /// This remains the ADJACENT-VALUE lane's class (the lane is narrower than
    /// the record route's and deliberately so), and the record route's
    /// baseline class. The record lane itself now runs the wider
    /// [`Self::is_morsel_eligible`] class; this predicate survives as the
    /// lane-specific fact and as the shared static-path fact the record
    /// receipts pin.
    #[must_use]
    pub fn is_morsel_static_path(&self) -> bool {
        self.program.is_morsel_static_path()
    }

    /// Whether this program may run once per record on a morsel worker: the
    /// WIDENED record-route eligibility.
    ///
    /// The clean-morsel law is program-independent — a morsel either publishes
    /// exactly what serial would for the same records or reports that it did
    /// not — so eligibility is the program's per-record OBSERVABILITY rather
    /// than its shape. A program whose every overload is
    /// [`jqf_builtins::registry::Effects::Pure`] cannot observe anything outside the
    /// one record in front of it, so its bytes on a worker are serial's bytes
    /// by construction. The input family, the stderr writers, `halt`, the
    /// nondeterministic builtins, and the engine surface (`~x` bindings and
    /// pulls) all decline.
    ///
    /// Widened per the parallel-differential clause: the standing
    /// differential's
    /// populations now include collect and fan-out shapes,
    /// `engaged == eligible` is asserted on both halves, and the record morsel
    /// tests pin a collect-shape relay.
    #[must_use]
    pub fn is_morsel_eligible(&self) -> bool {
        self.program.is_morsel_eligible()
    }

    /// Whether this program is the BARE IDENTITY filter (`.`, and nothing
    /// else): the root is a `Current` stage with zero steps.
    ///
    /// The source-preserving round-trip lane keys on this predicate: identity
    /// is the only program whose output is provably its input, so the only
    /// program whose bytes may be published without re-encoding. It is
    /// deliberately narrower than [`Self::is_morsel_static_path`], which also
    /// admits static Key/Index chains.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        self.program.is_identity()
    }

    /// Whether the program contains any assignment (`=`/`|=` family) node.
    ///
    /// The edit lane reads this to decide the output subject: a program with
    /// no assignment leaves the document unchanged, so edit mode publishes the
    /// retained input bytes; a program that assigns publishes the modified
    /// document.
    #[must_use]
    pub fn modifies(&self) -> bool {
        self.program.modifies()
    }

    /// Whether the program contains any FACT assignment (`ProgramNode::FactAssign`,
    /// `PATH.@comment = …` / `PATH.@comment_inline = …` /
    /// `PATH.@comment_foot = …` / `.&name` shapes).
    ///
    /// The edit lane applies the run's fact deltas as span operations against
    /// the retained source. Query-time overlay reads the same deltas. A program
    /// that also assigns a value merges those deltas with the value-diff patches
    /// in one apply under `--edit`.
    #[must_use]
    pub fn fact_writes(&self) -> bool {
        self.program
            .nodes()
            .iter()
            .any(|node| matches!(node, ProgramNode::FactAssign { .. }))
    }

    /// Whether the program reads or writes attached facts (`.@comment`,
    /// `.&href`, `json_facts`, a fact assignment).
    ///
    /// Construction sets [`jqf_codec_core::FactIntent::Preserve`] for these
    /// programs so a JSONC/JSON5 decode attaches the facts the run observes.
    #[must_use]
    pub fn accesses_facts(&self) -> bool {
        self.program.nodes().iter().any(|node| match node {
            ProgramNode::FactAssign { .. }
            | ProgramNode::Call {
                payload: Evaluator::JsonFacts,
                ..
            } => true,
            ProgramNode::Stage { steps, .. } => steps.iter().any(|step| {
                matches!(
                    step.access(),
                    StepAccess::NodeAccessor(_)
                        | StepAccess::Attribute(_)
                        | StepAccess::DynNodeAccessor(_)
                        | StepAccess::DynAttribute(_)
                )
            }),
            _ => false,
        })
    }

    /// Frontier for a count or element-iteration lowering.
    ///
    /// The policy override wins. A bounded element prefix (`limit`/`first`/`nth`)
    /// and every fan-out stay lazy so unread elements remain spans the JSON
    /// leaf can extract from. Unbounded count and reduce-fold default eager
    /// (`Some(0)`) so the prune hint is armed: JSON otherwise drops prune
    /// under a nonzero frontier.
    fn consumer_frontier(&self, stays_lazy: bool) -> Option<u32> {
        match self.policy.lazy_frontier() {
            Some(depth) => Some(depth),
            None if stays_lazy => None,
            None => Some(0),
        }
    }

    /// Whether this program's root evaluation consumes the entire input
    /// document — the eager side (see [`crate::analysis::lazy`]).
    ///
    /// The span-backed lazy representation pays extra time on
    /// touch-everything programs (identity, `..`, root-element folds,
    /// whole-document `|=`), so the CLI keeps those on the eager decode and
    /// offers the lazy default to everything else. Conservative: a program
    /// the analysis cannot prove partial is classified consuming.
    #[must_use]
    pub fn consumes_whole_document(&self) -> bool {
        let has_indexed_scans = !self.program.scans().is_empty() || !self.program.anti_joins().is_empty();
        crate::analysis::consumes_whole_document(self.program.nodes(), self.program.root(), has_indexed_scans)
    }

    /// The fused program arena, for crate-internal tests.
    #[cfg(test)]
    pub(crate) fn arena(&self) -> &[ProgramNode] {
        self.program.nodes()
    }

    /// The arena root, for crate-internal tests.
    #[cfg(test)]
    pub(crate) fn root(&self) -> crate::program::ProgramNodeId {
        self.program.root()
    }

    /// Interprets one codec access outcome into an engine-typed run under this
    /// program's per-component optional (`?`) flags and pushdown boundary.
    ///
    /// This is the typed run entry: the interpretation depends on the compiled
    /// program (its optional flags and its residual slice), so it lives on the
    /// compiled program. Everything flows through the residual — the executor
    /// always runs over an input item. A resolved value is that item; a
    /// pushed-down missing path or mismatch-on-null becomes an owned `null`
    /// input item (so `{} | .a[]` iterates null and errors, `{} | .a[]?`
    /// suppresses); a non-null mismatch at a flagged pushed-down step is
    /// [`EngineRun::Suppressed`] (no input exists); and any other non-null
    /// pushed-down mismatch is the typed [`EngineRun::Pushdown`] abort. The
    /// residual borrows this program's steps, so the returned run borrows the
    /// program.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] only when seeding the executor against the request
    /// ledger fails; the typed jq-parity arms are `Ok` variants.
    pub fn try_run<'source>(
        &self,
        outcome: CodecInputOutcome<'source>,
        resources: &ResourceContext<'_>,
    ) -> Result<EngineRun<'_, 'source>, CodecError> {
        // A count-class program lowered the LAZY WHOLE-DOCUMENT requirement:
        // the codec served the whole document, never the
        // pushed-down container, so a count-consumer decline runs the WHOLE
        // program over it — the pushdown prefix is applied by the engine, not
        // skipped as codec-resolved. The document-core consumer is the fast
        // path; this is the byte-identical fallback.
        if self.count_demand().is_some() {
            return self.try_run_whole_value(outcome, resources);
        }
        // The same for an element-iteration program: the
        // requirement was the LAZY whole document, so an element-consumer
        // decline runs the whole program over it.
        if self.element_demand().is_some() {
            return self.try_run_whole_value(outcome, resources);
        }
        // The mismatch dial beyond lenient: the codec never
        // resolved a pushed-down prefix (see [`Self::try_requirement`]), so
        // the WHOLE program runs from the root — the engine's own walk fires
        // every cell the reference evaluation would fire.
        if resources.mismatch_policy() != MismatchPolicy::Lenient {
            return self.try_run_whole_value(outcome, resources);
        }
        try_run(
            self.program.nodes(),
            self.program.root(),
            self.program.split().entry(),
            self.program.split().prefix_len(),
            self.program.entry_stage_steps(),
            self.program.slots(),
            self.program.scans(),
            self.program.topk_sorts(),
            self.program.anti_joins(),
            outcome,
            None,
        )
    }

    /// [`Self::try_run`] with the partial-sort table EXPLICITLY SELECTED — the
    /// recognizer differential's floor-forcing lever. Passing `&[]` runs the
    /// plain `sort`/`sort_by` evaluation (the forced floor) over the same
    /// arena; the real table runs the bounded-heap path.
    #[cfg(test)]
    pub(crate) fn try_run_with_table<'a, 'source>(
        &'a self,
        outcome: CodecInputOutcome<'source>,
        topk: &'a [crate::analysis::PartialSort],
    ) -> Result<EngineRun<'a, 'source>, CodecError> {
        try_run(
            self.program.nodes(),
            self.program.root(),
            self.program.split().entry(),
            self.program.split().prefix_len(),
            self.program.entry_stage_steps(),
            self.program.slots(),
            self.program.scans(),
            topk,
            self.program.anti_joins(),
            outcome,
            None,
        )
    }

    /// How much of one streamed document element this program consumes — the
    /// backward-demand classification, observed by receipts and the explain
    /// surface.
    ///
    /// It answers "how much of one element does this program read?" on
    /// the backward lattice `Structure < Fields(S) < Subtree`
    /// ([`ProjectionClass`]). Everything the
    /// transfer table does not pin classifies
    /// [`ProjectionClass::Subtree`], which is exactly today's behavior.
    ///
    /// The returned field names borrow this program's arena.
    #[must_use]
    pub fn projection_class(&self) -> ProjectionClass<'_> {
        self.program.projection_class()
    }

    /// Whether this program NAMES an element boundary, and what consumes its
    /// elements — the analysis fact.
    ///
    /// A named boundary is strictly wider than any single rung's eligibility:
    /// it reaches inside a `reduce`/`foreach` SOURCE and through `map`'s lowered
    /// `[.[] | f]` body, shapes no route may take. Naming changes no routing.
    #[must_use]
    pub fn element_boundary_consumer(&self) -> Option<BoundaryConsumer> {
        Some(self.program.element_boundary()?.consumer())
    }

    /// The recognized COUNT demand of this program, when it is a count-class
    /// row: the container path and per-item probe a
    /// document-core consumer can answer from a lazy document's span
    /// skeleton.
    ///
    /// `None` for every program the closed count table declines; the SDK
    /// drive then runs the program normally. The demand is derived ONCE at
    /// construction (the per-item drive consults it on every record, so a
    /// per-call derivation would re-run the arena walk and projection
    /// classification per streamed value); this accessor only reads the
    /// cached result.
    #[must_use]
    pub fn count_demand(&self) -> Option<&jqf_data::CountDemand> {
        self.count.as_ref()
    }

    /// The recognized ELEMENT-ITERATION demand of this program, when it is
    /// an element-iteration row (fan-out or reduce-fold): the
    /// container path and per-element probe a document-core consumer answers
    /// by iterating the lazy document's span skeleton — the count consumer's
    /// sibling. `None` for every program the closed element table declines;
    /// the SDK drive then runs the program normally. Derived ONCE at
    /// construction for the same reason as [`Self::count_demand`]; this
    /// accessor only reads the cached result.
    #[must_use]
    pub fn element_demand(&self) -> Option<&jqf_data::ElementDemand> {
        self.element.as_ref()
    }

    /// The static `(key, path)` pairs of a `PATH[] | {static keys}`
    /// element-iteration row. `None` when the program is not that row.
    #[must_use]
    pub fn element_construct_fields(&self) -> Option<&[(alloc::string::String, Vec<jqf_data::CountStep>)]> {
        self.element_construct.as_deref()
    }

    /// Whether the recognized element-iteration row is `[FAN-OUT]`: the
    /// SDK collects every visited probe value into one array.
    #[must_use]
    pub const fn element_collect(&self) -> bool {
        self.element_collect
    }

    /// One construction epilogue: everything a compiled program needs
    /// after [`Program::new`], owned here so both compile entries — the
    /// ordinary lowering and the arena-level slurp-aggregate rewrite —
    /// cannot drift apart. Route-class facts (`count`, `element`, `keys`,
    /// `type_path`, prune trees) derive here once; per-record consumers read
    /// the cached fields.
    ///
    /// Prune law, resolved deliberately: BOTH trees are derived
    /// UNCONDITIONALLY and [`crate::analysis::prune_trees`] declines on its
    /// own walk conditions. The rewrite path historically gated the root tree
    /// on `split.is_whole_document()`; that gate was a no-op there — the
    /// rewrite's root is always `reduce inputs as $x (…)`, whose source pull
    /// makes the prune walker record an input pull, and `prune_trees`
    /// blanket-declines any program that pulls (`analysis/prune.rs`). One
    /// law for both callers: derive, and let the analysis decline.
    ///
    /// The caller keeps its own pre-`Program::new` marks (tail calls,
    /// keyed collects, binary shapes, static object keys): the rewrite path
    /// hand-builds its nodes with explicit shape fields and runs only the
    /// keyed-collect mark, and those walks mutate the node vector, which
    /// this constructor takes as an already-fused `Program`.
    pub(crate) fn finish(
        program: Program,
        policy: CodecRequirementPolicy,
        uses_inputs_cursor: bool,
        runtime_index_slot: Option<u32>,
    ) -> Self {
        let RouteDemands {
            count,
            element,
            element_construct,
            element_collect,
        } = Self::derive_route_demands(&program);
        // Derived for every program whose reads bound below the whole
        // document: whole-document lowerings attach the tree at the root,
        // and a static forward prefix re-anchors it onto the located node.
        let (prune, pulled_record) = crate::analysis::prune_trees(program.nodes(), program.root(), program.slots());
        let keys = crate::analysis::count::keys_demand(program.nodes(), program.root());
        let type_path = crate::analysis::count::type_demand_path(program.nodes(), program.root());
        Self {
            program,
            policy,
            prune,
            uses_inputs_cursor,
            runtime_index_slot,
            count,
            element,
            element_construct,
            element_collect,
            keys,
            type_path,
            pulled_record,
        }
    }

    /// Derives count and element route demands in one epilogue: the projection
    /// classification is computed once and shared with the count table.
    fn derive_route_demands(program: &crate::program::Program) -> RouteDemands {
        #[cfg(test)]
        let _guard = DERIVATION_LOCK.lock().expect("derivation lock");
        #[cfg(test)]
        {
            COUNT_DEMAND_DERIVATIONS.fetch_add(1, Ordering::Relaxed);
            ELEMENT_DEMAND_DERIVATIONS.fetch_add(1, Ordering::Relaxed);
        }
        let class = program.projection_class();
        let count = crate::analysis::count::count_demand(program.nodes(), program.root(), program.split(), &class);
        let plan = crate::analysis::element::element_plan(program.nodes(), program.root(), program.split());
        let (element, element_construct, element_collect) = match plan {
            Some(plan) => (Some(plan.demand), plan.construct, plan.collect),
            None => (None, None, false),
        };
        RouteDemands {
            count,
            element,
            element_construct,
            element_collect,
        }
    }

    /// Whether this program is a type-class row: a kind-only read of the root
    /// (`type`) or of a static path (`PATH | type`) with no trailing slice.
    /// `false` for `type?` and for `.[1:3] | type`.
    #[must_use]
    pub fn type_demand(&self) -> bool {
        self.type_path.is_some()
    }

    /// The static path a type-class program names. Empty is the document root.
    /// `None` when [`Self::type_demand`] is false.
    #[must_use]
    pub(crate) fn type_demand_path(&self) -> Option<&[jqf_data::CountStep]> {
        self.type_path.as_deref()
    }

    /// The static path a keys-publish program names (`keys` / `PATH | keys`).
    /// Empty is the document root. `None` when the program is not that row.
    #[must_use]
    pub fn keys_demand(&self) -> Option<&[jqf_data::CountStep]> {
        self.keys.as_deref()
    }

    /// Whether this program is the RANGE-locate row `PATH[a:b]` — a bare slice
    /// PUBLISH — so the codec's scoped route can serve it by cutting the byte
    /// region that holds exactly the in-range elements.
    ///
    /// It is DISJOINT from every other rung by construction: the row's root is a
    /// bare `Stage` ending in a slice, so it names no `.[]` boundary (which the
    /// count demand and the other boundary-consuming rows all require) and it
    /// is not the `PATH[a:b] | length` pipe the count table matches.
    ///
    /// The soundness argument is the locate table's own
    /// (`analysis::is_range_locate`). The DECLINE half of it — the
    /// container dispatch — is the consuming drive's: nothing is published
    /// unless the codec resolved an ARRAY at the range step, so a string, null,
    /// missing or object container reaches the floor's own answer through the
    /// ordinary route.
    #[must_use]
    pub fn range_locate_eligible(&self) -> bool {
        self.program.range_locate()
    }

    /// How many partial-sort rows of the CLOSED `top_k` table this program's
    /// graph matches (the recognizer door, later widened). The
    /// executor answers each row with the bounded-heap partial sort instead of
    /// a full sort plus slice; the count is the routing fact `--explain` renders
    /// and the lane-recognition suite pins — the substitution itself records
    /// nothing at run time, so the recognizer's verdict must be assertable at
    /// compile time or a row that silently stops matching is invisible.
    #[must_use]
    pub fn topk_rows(&self) -> usize {
        self.program.topk_sorts().len()
    }

    /// Lowers the RANGE-locate row's exact path (its static prefix plus the one
    /// trailing range) into an exact codec LOCATED requirement.
    ///
    /// The footprint and schedule are byte-identical to the range-COUNT
    /// lowering of the same path; only the result authority differs, so the
    /// bare-slice publish shares the range row's validate/locate/resolve work
    /// exactly as the P0 rung shares the scoped route's.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when called on an ineligible program, when a bound
    /// fails the normalization law, or when charging the lowered requirement
    /// against the request ledger fails.
    pub fn try_range_locate_requirement(
        &self,
        resources: &ResourceContext<'_>,
    ) -> Result<AccessRequirement, CodecError> {
        let ineligible = || {
            CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "range-locate requirement for an ineligible program",
            })
        };
        if !self.program.range_locate() {
            return Err(ineligible());
        }
        let steps = self.program.stage_steps(self.program.root());
        let mut path = Vec::new();
        path.try_reserve_exact(steps.len())
            .map_err(|_| CodecError::new(CodecFailureKind::AllocationFailure))?;
        for step in steps {
            path.push(match step.access() {
                StepAccess::Key(key) => StaticForwardStep::ObjectKey(key.as_str()),
                StepAccess::Index(index) => StaticForwardStep::ArrayIndex(*index),
                StepAccess::Slice(bounds) => {
                    let (start, end) = bounds.try_normalize().ok_or_else(ineligible)?;
                    StaticForwardStep::ArrayRange { start, end }
                }
                StepAccess::Each
                | StepAccess::DynVar(_)
                | StepAccess::Descend
                | StepAccess::NodeAccessor(_)
                | StepAccess::Attribute(_)
                | StepAccess::DynNodeAccessor(_)
                | StepAccess::DynAttribute(_) => {
                    return Err(ineligible());
                }
            });
        }
        try_lower_forward_requirement(self.policy, &path, resources)
    }
}
