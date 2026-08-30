//! Static-path pipe/optional/comma/group program compilation over the shared
//! syntax frontend.
//!
//! This is the CLI-to-engine seam. [`try_compile_program`] runs the compile
//! pipeline — parse, lower to a [`crate::program`] arena, fuse the graph to
//! path-normal form and analyze the codec-pushdown split, charge the arena — and
//! produces an opaque [`CompiledProgram`]. [`CompiledProgram::try_requirement`]
//! lowers the derived split into a codec [`AccessRequirement`], reusing the
//! existing tested requirement lowerings; it invents no new requirement
//! vocabulary. [`CompiledProgram::try_run`] carries the program's optional (`?`)
//! flags and its residual graph into the engine's typed run interpretation.
//!
//! This module owns [`EngineCompileError`] and the pipeline orchestration;
//! [`ParseRejection`]/[`UnsupportedConstruct`] live in
//! `jqf_builtins::constant` and are re-exported here so the
//! public `jqf_engine::` surface is unchanged.
//! It does not lower an AST (that is [`lower`]), fuse or derive analysis facts
//! (that is [`crate::analysis`]), store the arena (that is [`crate::program`]),
//! build codec requirements (that is [`crate::codec_requirement`]), or interpret
//! run outcomes (that is [`crate::exec`]).

mod lower;

pub(crate) use jqf_builtins::constant::try_copy_str;
pub use jqf_builtins::constant::{ParseRejection, UnsupportedConstruct};

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

#[cfg(test)]
extern crate std;
#[cfg(test)]
use core::sync::atomic::{AtomicUsize, Ordering};

use jqf_codec_core::{AccessRequirement, CodecError, CodecFailureKind, DemandClause};
use jqf_resource::policy::MismatchPolicy;
use jqf_resource::{ResourceContext, ResourceError};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef, Span};
use jqf_syntax::{SyntaxInputError, parse_program, parse_query};

use jqf_data::{ExpandedName, Value};

use crate::analysis::{BoundaryConsumer, ProjectionClass, analyze, fuse_with_callables};
use crate::codec_requirement::{
    CodecRequirementPolicy, StaticForwardStep, try_lower_forward_requirement, try_lower_prune_tree,
    try_lower_root_requirement,
};
use crate::exec::{EngineRun, try_run};
use crate::program::{BinaryKind, Program, ProgramNode, ProgramNodeId, StageStart, StageStep, StepAccess};
use jqf_builtins::codec_result::CodecInputOutcome;
use jqf_builtins::registry::{Evaluator, resolve_builtin};

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
static COUNT_DEMAND_DERIVATIONS: AtomicUsize = AtomicUsize::new(0);

/// How many times the recognized element-iteration demand has been DERIVED
/// (the fan-out/reduce-fold arena walk) rather than read from the cached
/// field — test-only: the derivation-cache regression test's element twin.
#[cfg(test)]
static ELEMENT_DEMAND_DERIVATIONS: AtomicUsize = AtomicUsize::new(0);

/// The test-only lock that makes the derivation counters race-free (see
/// [`COUNT_DEMAND_DERIVATIONS`]). Tests build in the `std` world even though
/// the crate is `no_std`, so the mutex is available here.
#[cfg(test)]
static DERIVATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Maps the constant-evaluation failure channel onto the engine's compile
/// error (`jqf_builtins::constant` owns the three variants
/// constant evaluation constructs; `lower.rs`'s `?` sites convert through
/// this).
impl From<jqf_builtins::constant::ConstantEvalError> for EngineCompileError {
    fn from(error: jqf_builtins::constant::ConstantEvalError) -> Self {
        match error {
            jqf_builtins::constant::ConstantEvalError::Parse(rejection) => Self::Parse(rejection),
            jqf_builtins::constant::ConstantEvalError::Unsupported { span, construct } => {
                Self::Unsupported { span, construct }
            }
            jqf_builtins::constant::ConstantEvalError::Resource(error) => Self::Resource(error),
        }
    }
}

/// A recognized static-path pipe/optional/comma/group program ready to lower into
/// a codec requirement and interpret run outcomes.
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
    program: Program,
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
    /// ([`try_compile_split_program`]), seeded per run with the item counter
    /// ([`Self::try_run_split`]). `None` for every ordinary program and for a
    /// split expression that never references `$index`.
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
    /// Kept-subtree selection over a pulled `input`/`inputs` record.
    /// Present when the program pulls and the pulled demand bounds below
    /// the whole record. The record `NullFirst` drive attaches it as a
    /// per-record hint; the root prune tree stays declined.
    pulled_record: Option<crate::analysis::PruneNode>,
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
            let requirement =
                match crate::analysis::prune_tree(self.program.nodes(), self.program.root(), self.program.slots()) {
                    Some(tree) => requirement.with_prune(try_lower_prune_tree(&tree, resources)?),
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
        if self.type_demand_path().is_some_and(|path| path.is_empty()) {
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
    /// time through the `inputs` cursor and is byte-identical (jq's `map` is
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
        // `null` is jq's neutral `+` identity for both numbers and strings
        // (the add aggregate's init); the length aggregate's init is the
        // exact integer zero.
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
    fn reanchor_prune_keys(&self) -> Option<Vec<String>> {
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

    /// The one construction epilogue: everything a compiled program needs
    /// after [`Program::new`], owned here so both compile entries — the
    /// ordinary lowering and the arena-level slurp-aggregate rewrite —
    /// cannot drift apart. A fourth derived fact added tomorrow has exactly
    /// one place to be derived and one set of fields to be assembled into.
    ///
    /// Prune law, resolved deliberately: BOTH trees are derived
    /// UNCONDITIONALLY and `analysis::prune_tree` declines on its own walk
    /// conditions. The rewrite path historically gated the root tree on
    /// `split.is_whole_document()`; that gate was a no-op there — the
    /// rewrite's root is always `reduce inputs as $x (…)`, whose source pull
    /// makes the prune walker record an input pull, and `prune_tree`
    /// blanket-declines any program that pulls (`analysis/prune.rs`). One
    /// law for both callers: derive, and let the analysis decline.
    ///
    /// The caller keeps its own pre-`Program::new` marks (tail calls,
    /// keyed collects, binary shapes, static object keys): the rewrite path
    /// hand-builds its nodes with explicit shape fields and runs only the
    /// keyed-collect mark, and those walks mutate the node vector, which
    /// this constructor takes as an already-fused `Program`.
    fn finish(
        program: Program,
        policy: CodecRequirementPolicy,
        uses_inputs_cursor: bool,
        runtime_index_slot: Option<u32>,
    ) -> Self {
        // The count and element demands are compile-time constants of the
        // immutable program, derived ONCE here: the per-item drive consults
        // them on every record (count_answer, element_answer, and the
        // residual run's decline probes), and a per-record re-derivation
        // would charge the arena walk and the projection classification to
        // every streamed value.
        let count = Self::derive_count_demand(&program);
        let plan = Self::derive_element_plan(&program);
        let (element, element_construct, element_collect) = match plan {
            Some(plan) => (Some(plan.demand), plan.construct, plan.collect),
            None => (None, None, false),
        };
        // Derived for every program whose reads bound below the whole
        // document: whole-document lowerings attach the tree at the root,
        // and a static forward prefix re-anchors it onto the located node.
        let prune = crate::analysis::prune_tree(program.nodes(), program.root(), program.slots());
        let pulled_record = crate::analysis::pulled_record_prune_tree(program.nodes(), program.root(), program.slots());
        let keys = crate::analysis::count::keys_demand(program.nodes(), program.root());
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
            pulled_record,
        }
    }

    /// Derives the recognized count demand for a fresh program — the arena
    /// walk plus the projection classification. Runs exactly ONCE per
    /// compiled program, at construction: the per-item drive consults the
    /// cached result ([`Self::count_demand`]) on every record, so re-deriving
    /// per consultation would charge every streamed value for a fact that
    /// cannot change between them.
    fn derive_count_demand(program: &crate::program::Program) -> Option<jqf_data::CountDemand> {
        #[cfg(test)]
        {
            let _guard = DERIVATION_LOCK.lock().expect("derivation lock");
            COUNT_DEMAND_DERIVATIONS.fetch_add(1, Ordering::Relaxed);
        }
        crate::analysis::count::count_demand(
            program.nodes(),
            program.root(),
            program.split(),
            &program.projection_class(),
        )
    }

    /// Derives the recognized element-iteration demand for a fresh program —
    /// the fan-out/reduce-fold arena walk. Runs exactly ONCE per compiled
    /// program, at construction (see [`Self::derive_count_demand`]).
    fn derive_element_plan(program: &crate::program::Program) -> Option<crate::analysis::element::ElementPlan> {
        #[cfg(test)]
        {
            let _guard = DERIVATION_LOCK.lock().expect("derivation lock");
            ELEMENT_DEMAND_DERIVATIONS.fetch_add(1, Ordering::Relaxed);
        }
        crate::analysis::element::element_plan(program.nodes(), program.root(), program.split())
    }

    /// Whether this program is a type-class row: a kind-only read of the root
    /// (`type`) or of a static path (`PATH | type`) with no trailing slice.
    /// `false` for `type?` and for `.[1:3] | type`.
    #[must_use]
    pub fn type_demand(&self) -> bool {
        crate::analysis::count::type_demand(self.program.nodes(), self.program.root())
    }

    /// The static path a type-class program names. Empty is the document root.
    /// `None` when [`Self::type_demand`] is false.
    #[must_use]
    pub(crate) fn type_demand_path(&self) -> Option<alloc::vec::Vec<jqf_data::CountStep>> {
        crate::analysis::count::type_demand_path(self.program.nodes(), self.program.root())
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
    /// is not the
    /// `PATH[a:b] | length` pipe [`Self::range_count_eligible`] matches.
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

/// Structured failure to compile a program into the bootstrap subset.
#[derive(Debug)]
#[non_exhaustive]
pub enum EngineCompileError {
    /// Program source exceeded the syntax input bound before parsing.
    Input(SyntaxInputError),
    /// The syntax parser reported one or more diagnostics.
    Parse(ParseRejection),
    /// A recognized construct is outside the static-path bootstrap subset.
    Unsupported {
        /// Byte span of the rejected construct.
        span: Span,
        /// The named construct that is not part of the subset.
        construct: UnsupportedConstruct,
    },
    /// A call named no registered builtin overload at its arity (the
    /// `name/arity is not defined` compile error). This is a resolution failure,
    /// not an out-of-subset rejection: the call grammar is recognized, but no
    /// `(name, arity)` matches the registry.
    UndefinedCall {
        /// Byte span of the call name.
        span: Span,
        /// The unresolved call name, as authored.
        name: String,
        /// The call's authored arity.
        arity: u8,
    },
    /// A call whose argument count passes the registry's one-byte arity
    /// ceiling. No overload can ever resolve at such a count, so the
    /// rejection names the AUTHORED count rather than a clamped one.
    ArityLimit {
        /// Byte span of the call name.
        span: Span,
        /// The call name, as authored.
        name: String,
        /// The authored argument count.
        count: u32,
    },
    /// A `$x` reference with no enclosing binder (the `$x is not defined`
    /// compile error, exit-3 class). Like [`Self::UndefinedCall`] this is a
    /// RESOLUTION failure, not an out-of-subset rejection: the grammar is
    /// recognized, but no lexical scope binds the name.
    UndefinedVariable {
        /// Byte span of the variable reference.
        span: Span,
        /// The unresolved variable name, as authored (including the `$`).
        name: String,
    },
    /// A `~x` engine-binding reference with no enclosing `as ~x` binder.
    /// Resolution failure in the ENGINE namespace, kept distinct from
    /// [`Self::UndefinedVariable`] because the two namespaces never collide.
    UndefinedEngineBinding {
        /// Byte span of the engine-binding reference, including the `~`.
        span: Span,
        /// The unresolved engine-binding name, as authored (including the `~`).
        name: String,
    },
    /// A BARE engine binding `~x` reached a VALUE position: `~x` alone, `[~x]`,
    /// `~x | .`, `~x | tostring`. The engine binding never crosses into the
    /// value domain — the only projections that lower to values are `~x.next`
    /// and `~x.rest`, and this guard is the reason the boundary is un-crossable
    /// by construction.
    EngineBindingAsValue {
        /// Byte span of the offending reference, including the `~`.
        span: Span,
        /// The engine-binding name, as authored (including the `~`).
        name: String,
    },
    /// A postfix chain on an engine binding that is not exactly one of the two
    /// protocol projections: `~x.next.foo`, `~x[0]`, `~x.next[0]`. The protocol
    /// is CLOSED — `.next` and `.rest` are the only projections that exist, and
    /// each is a complete expression on its own.
    EngineBindingProjection {
        /// Byte span of the offending postfix chain.
        span: Span,
        /// The engine-binding name, as authored (including the `~`).
        name: String,
    },
    /// An engine-constructor call `~name(...)` whose name is not a registered
    /// engine constructor (the constructor list is CLOSED, and the only
    /// resident is `~generator`).
    UndefinedEngineConstructor {
        /// Byte span of the constructor reference, including the `~`.
        span: Span,
        /// The unresolved constructor name, as authored.
        name: String,
    },
    /// An engine constructor bound to a VALUE pattern, or an engine binding
    /// whose value is not an engine constructor: `~generator(...) as $x` and
    /// `(1,2) as ~x`. The two-mark rule — both the binding AND the constructor
    /// carry `~` — makes the value/engine boundary un-crossable by construction.
    EngineBindingShape {
        /// Byte span of the offending form.
        span: Span,
        /// Why the form is rejected, rendered into the message.
        reason: alloc::string::String,
    },
    /// A pull of an engine binding from inside a RECURSIVE user `def` body (the
    /// carve-out): the callable body runs on a NESTED evaluator
    /// with no cursor store, so the pull is rejected at COMPILE time — never a
    /// hang, never a silent wrong answer. The loop idiom is written with
    /// `while`/`until`/`repeat`/`recurse` instead, which are frame-machine
    /// generators and can pull.
    EnginePullInRecursiveDef {
        /// Byte span of the offending pull.
        span: Span,
        /// The engine-binding name, as authored (including the `~`).
        name: String,
    },
    /// An engine-binding reference inside a `~generator` constructor argument:
    /// the constructor's cursor is a SEPARATELY-OWNED machine, and its graphs
    /// cannot capture a cursor owned by the enclosing machine (cross-machine
    /// capture is impossible without a shared cursor store).
    EngineBindingInConstructor {
        /// Byte span of the offending reference.
        span: Span,
        /// The engine-binding name, as authored (including the `~`).
        name: String,
    },
    /// An engine binding used as a `reduce`/`foreach` loop pattern (`reduce SRC
    /// as ~x (...)`): loops bind VALUES; only `as ~x` introduces a cursor.
    EngineBindingLoopPattern {
        /// Byte span of the offending pattern.
        span: Span,
    },
    /// A `break $x` with no enclosing `label $x` (the `$*label-x is not
    /// defined` compile error, exit-3 class). Kept distinct from
    /// [`Self::UndefinedVariable`] because labels are a SEPARATE namespace: it
    /// is named `$*label-x`, and reporting it as a plain variable would misstate
    /// which stack the lookup missed.
    UndefinedLabel {
        /// Byte span of the label reference.
        span: Span,
        /// The unresolved label name, as authored (including the `$`).
        name: String,
    },
    /// Program structure nested deeper than the lowering walk will descend.
    ///
    /// The parser bounds the tree it builds at the same ceiling, so this is
    /// what catches the trees lowering reaches by another route: a `def` body
    /// is re-lowered at every call site, and a filter argument is re-lowered
    /// inside the callee, so a shallow-looking program can still present a
    /// deeper walk than any single parse did.
    ///
    /// It is a statement about the PROGRAM (the exit-3 class), not about the
    /// host's resources, which is why it is not a [`Self::Resource`] arm even
    /// though it shares the ledger's ceiling and its wording.
    NestingTooDeep {
        /// Byte span of the expression one level past the ceiling.
        span: Span,
        /// The ceiling, in levels.
        limit: u32,
    },
    /// The request ledger rejected charging the compiled arena.
    Resource(ResourceError),
}

impl EngineCompileError {
    fn unsupported(span: Span, construct: UnsupportedConstruct) -> Self {
        Self::Unsupported { span, construct }
    }

    fn undefined_call(span: Span, name: &str, arity: u8) -> Self {
        Self::UndefinedCall {
            span,
            name: try_copy_str(name).unwrap_or_else(|| String::from("<call>")),
            arity,
        }
    }

    fn arity_limit(span: Span, name: &str, count: u32) -> Self {
        Self::ArityLimit {
            span,
            name: try_copy_str(name).unwrap_or_else(|| String::from("<call>")),
            count,
        }
    }

    fn undefined_variable(span: Span, name: &str) -> Self {
        Self::UndefinedVariable {
            span,
            name: try_copy_str(name).unwrap_or_else(|| String::from("$<variable>")),
        }
    }

    fn undefined_engine_binding(span: Span, name: &str) -> Self {
        Self::UndefinedEngineBinding {
            span,
            name: try_copy_str(name).unwrap_or_else(|| String::from("~<binding>")),
        }
    }

    fn engine_binding_as_value(span: Span, name: &str) -> Self {
        Self::EngineBindingAsValue {
            span,
            name: try_copy_str(name).unwrap_or_else(|| String::from("~<binding>")),
        }
    }

    fn engine_binding_projection(span: Span, name: &str) -> Self {
        Self::EngineBindingProjection {
            span,
            name: try_copy_str(name).unwrap_or_else(|| String::from("~<binding>")),
        }
    }

    fn engine_pull_in_recursive_def(span: Span, name: &str) -> Self {
        Self::EnginePullInRecursiveDef {
            span,
            name: try_copy_str(name).unwrap_or_else(|| String::from("~<binding>")),
        }
    }

    fn engine_binding_in_constructor(span: Span, name: &str) -> Self {
        Self::EngineBindingInConstructor {
            span,
            name: try_copy_str(name).unwrap_or_else(|| String::from("~<binding>")),
        }
    }

    fn undefined_label(span: Span, name: &str) -> Self {
        Self::UndefinedLabel {
            span,
            name: try_copy_str(name).unwrap_or_else(|| String::from("$<label>")),
        }
    }

    /// The program-source span the rejection points at, when one exists.
    ///
    /// Every rejection ABOUT the program text carries one; the two arms with
    /// none are statements about the input size and the host's resources.
    /// This is the accessor a presenter (the CLI's caret excerpt) reads —
    /// the Display text keeps the byte offsets, so a consumer with no source
    /// in hand loses nothing.
    #[must_use]
    pub const fn span(&self) -> Option<Span> {
        match self {
            Self::Input(_) | Self::Resource(_) => None,
            Self::Parse(rejection) => rejection.span(),
            Self::Unsupported { span, .. }
            | Self::UndefinedCall { span, .. }
            | Self::ArityLimit { span, .. }
            | Self::UndefinedVariable { span, .. }
            | Self::UndefinedEngineBinding { span, .. }
            | Self::EngineBindingAsValue { span, .. }
            | Self::EngineBindingProjection { span, .. }
            | Self::UndefinedEngineConstructor { span, .. }
            | Self::EngineBindingShape { span, .. }
            | Self::EnginePullInRecursiveDef { span, .. }
            | Self::EngineBindingInConstructor { span, .. }
            | Self::EngineBindingLoopPattern { span, .. }
            | Self::UndefinedLabel { span, .. }
            | Self::NestingTooDeep { span, .. } => Some(*span),
        }
    }
}

/// The standard library, as SOURCE.
///
/// Every definition here is transcribed source, verified by the corpus.
/// Writing them as source rather than as hand-built arena graphs is the point:
/// the semantics come from the real definition instead of a reimplementation,
/// which is how the `limit`/`first` edge-case table came out right by
/// construction.
///
/// Two rules govern what may live here, and both are about failing early rather
/// than late:
///
/// 1. **Non-recursive only.** `def` is inlining-only, so a recursive definition
///    (`until`, `while`, `repeat`, `recurse`) would be rejected at its first
///    call site — a worse failure than not offering it at all.
/// 2. **Only vocabulary the engine already has.** A definition written against
///    an unregistered builtin compiles fine as PRELUDE (nothing lowers until it
///    is called) and then fails at the call site; the `type`-based selectors
///    joined this list the moment `type/0` was registered, which is exactly the
///    intended cadence.
///
/// `last(g)` is spelled `[g] | .[-1:] | .[]` rather than a fold: the original
/// definition was not recoverable, and the observable law is that
/// `last(empty)` emits NOTHING. A `reduce g as $x (null; $x)` would emit
/// `null` instead, which was an earlier attempt and was caught.
///
/// The trailing `.` is the query the prelude parse needs to be a complete
/// program; it is discarded — only the `def` chain is read.
const STDLIB_PRELUDE: &str = "\
def isempty(g): first((g|false), true);\n\
def all(generator; condition): isempty(generator|condition and empty);\n\
def any(generator; condition): isempty(generator|condition or empty)|not;\n\
def all(condition): all(.[]; condition);\n\
def any(condition): any(.[]; condition);\n\
def all: all(.[]; .);\n\
def any: any(.[]; .);\n\
def first: .[0];\n\
def last: .[-1];\n\
def last(g): [g] | .[-1:] | .[];\n\
def values: select(. != null);\n\
def nulls: select(. == null);\n\
.";

/// Every name [`STDLIB_PRELUDE`] defines, for the substring gate above.
///
/// This list must stay in step with the prelude. A name here that the prelude
/// does not define only costs a wasted parse; a prelude definition MISSING from
/// here is a correctness bug — its calls would report `not defined` — so the
/// unit test below pins the two together.
const STDLIB_NAMES: &[&str] = &["isempty", "all", "any", "first", "last", "values", "nulls"];

/// Every name [`EXTENSION_PRELUDE`] defines, for the same substring gate.
const EXTENSION_NAMES: &[&str] = &[
    "windows",
    "moving_sum",
    "moving_avg",
    "moving_min",
    "moving_max",
    "ewma",
    "deltas",
    "lag",
    "running",
    "counter",
];

/// The EXTENSION prelude: the window/rolling builtin family, as SOURCE
/// definitions beside the transcribed standard surface.
///
/// Every name here is an extension — none of them exists in the reference
/// surface (the `builtins` diff is the gate), so the stdlib const above stays
/// pure by rule and this one carries the extension surface. The prelude rules
/// bind identically: non-recursive `foreach`/`reduce` compositions only, and
/// every name they call is registered vocabulary.
///
/// The family is generator-parameterized (`limit($n; g)` shape): the stream is
/// an explicit generator argument, never `inputs`, so it composes with arrays,
/// `inputs`, and `--follow` alike. Argument order is `($param; g)` — the
/// window/alpha first, matching `limit/2`.
///
/// Spelling notes:
/// - `deltas`/`lag` share the `{p, first}` state shape; the first input emits
///   nothing, so the update computes `$x - .p` only when `.first` is false
///   (the original sketch subtracted on the first iteration, which raises
///   `number and null cannot be subtracted`).
/// - `running(f; g)` is the call-by-name law: the filter argument is inlined
///   in the CALLER's scope, so `f` sees the state as `.` and can never
///   reference the foreach element `$x` (`running(. + $x; g)` is rejected with
///   `$x is not defined`). The scan is state-only by construction.
/// - `moving_min`/`moving_max` recompute over the window array: O(n) per step
///   is the v1 ceiling; the monotonic-deque trick is the recognizer-era
///   upgrade (ponytail: ceiling documented, upgrade path named).
/// - Exact-decimal accumulation makes `moving_sum`/`moving_avg` drift-free
///   where a float accumulator drifts.
///
/// The trailing `.` is the query the prelude parse needs to be a complete
/// program; it is discarded — only the `def` chain is read.
const EXTENSION_PRELUDE: &str = "\
def windows($n; g): foreach g as $x ([]; (. + [$x])[-$n:]);\n\
def moving_sum($n; g): foreach g as $x ({q: [], s: 0}; .q += [$x] | .s += $x | if (.q|length) > $n then .s -= .q[0] | .q |= .[1:] else . end; .s);\n\
def moving_avg($n; g): foreach g as $x ({q: [], s: 0}; .q += [$x] | .s += $x | if (.q|length) > $n then .s -= .q[0] | .q |= .[1:] else . end; .s / (.q|length));\n\
def moving_min($n; g): foreach g as $x ([]; (. + [$x])[-$n:]; min);\n\
def moving_max($n; g): foreach g as $x ([]; (. + [$x])[-$n:]; max);\n\
def ewma($a; g): foreach g as $x (null; if . == null then $x else $a * $x + (1 - $a) * . end);\n\
def deltas(g): foreach g as $x ({p: null, first: true}; {p: $x} + (if .first then {first: false, skip: true} else {first: false, d: ($x - .p)} end); if .skip then empty else .d end);\n\
def lag(g): foreach g as $x ({p: null, first: true}; {p: $x, first: false} + (if .first then {skip: true} else {v: .p} end); if .skip then empty else .v end);\n\
def running(f; g): foreach g as $x (null; f);\n\
def counter(g): foreach g as $x (0; . + 1);\n\
.";

/// The SYNTAX half of the rejection message's capability enumeration.
///
/// It states what the engine actually runs, in the order the grammar landed
/// it. It is prose because lowering has no single source of truth to generate
/// it from — `lower_expr` is a match over the AST, not a table — so it is
/// guarded instead: `the_message_names_only_forms_that_compile` compiles one
/// probe program per named form and fails if a claim here has stopped being
/// true. A vertical that widens the grammar widens this string in the same
/// commit, and one that narrows it is caught by the probe.
///
/// Deliberately NOT listed, because it is still rejected at lower time: a
/// `?//` chain in a THREE-argument `foreach`.
const SUPPORTED_SYNTAX: &str = "identity `.`, field and index paths (`.a`, `.\"k\"`, `.[\"k\"]`, \
     `.[0]`, `.[-1]`) with per-component `?`, `.[]` iteration, recursive descent `..`, slices \
     `.[e1:e2]`, dynamic indexing `.[e]`, unary negation `-`, \
     pipe `|`, comma `,`, \
     parenthesized groups, literals, `[…]`/`{…}` constructors, arithmetic `+ - * / %`, \
     comparisons `== != < <= > >=`, `and`/`or`, the alternative `//`, `if`/`elif`/`else`, \
     `try`/`catch` and its `?` sugar, `as` bindings over a variable or a \
     destructuring pattern (`as [$a,$b]`, `as {a:$v,$b}`) and its `?//` \
     alternative chain, `reduce`, `foreach`, \
     `label $out`/`break $out`, and assignment `=`, update `|=`, and the \
     arithmetic/alternative updates `+= -= *= /= %= //=`";

/// The BUILTIN half of the same enumeration, GENERATED from the registry.
///
/// This is deliberately not prose. `registry::builtin_overloads()` is the
/// single source of truth for which `(name, arity)` pairs resolve, so rendering
/// from it makes the clause honest by construction: a vertical that registers
/// an overload widens this message with no second edit, and the message can
/// never again claim a narrower surface than the engine has. The stale
/// enumeration this replaced named four builtins when seven resolved.
struct RegisteredBuiltins;

impl fmt::Display for RegisteredBuiltins {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (position, overload) in jqf_builtins::registry::builtin_overloads().iter().enumerate() {
            if position > 0 {
                formatter.write_str(", ")?;
            }
            write!(formatter, "`{}/{}`", overload.canonical_name, overload.arity)?;
        }
        Ok(())
    }
}

#[allow(clippy::too_many_lines)]
impl fmt::Display for EngineCompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(error) => write!(formatter, "cannot parse program: {error}"),
            Self::Parse(rejection) => match rejection.span() {
                Some(span) => write!(
                    formatter,
                    "cannot parse program at bytes {}..{}: {}",
                    span.start(),
                    span.end(),
                    rejection.message()
                ),
                None => write!(formatter, "cannot parse program: {}", rejection.message()),
            },
            Self::Unsupported { span, construct } => {
                // Every rejection that names ONE exact spelling leads with its
                // own sentence and skips the supported-surface dump — a
                // multi-hundred-word wall is the worst first contact with the
                // first carve-out; the same law now covers
                // the accessor family, whose `describe()`
                // texts name the construct and the spellings that DO compile
                // (`jqf --help facts` is the discovery surface for the
                // accessor spellings). Only the generic `Expression(name)`
                // forms keep the full enumeration, which frames an unexpected
                // rejection with what IS supported.
                if matches!(*construct, UnsupportedConstruct::AccessorAssignment) {
                    return write!(
                        formatter,
                        "unsupported construct at bytes {}..{}: {}",
                        span.start(),
                        span.end(),
                        construct.describe()
                    );
                }
                write!(
                    formatter,
                    "unsupported construct at bytes {}..{}: {} is outside the supported \
                     surface ({SUPPORTED_SYNTAX}; builtins: {RegisteredBuiltins})",
                    span.start(),
                    span.end(),
                    construct.describe()
                )
            }
            Self::UndefinedCall { span, name, arity } => write!(
                formatter,
                "{name}/{arity} is not defined at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            Self::ArityLimit { span, name, count } => write!(
                formatter,
                "{name}/{count} is not defined: a call can have at most 255 arguments \
                 at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            Self::UndefinedVariable { span, name } => write!(
                formatter,
                "{name} is not defined at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            // The engine namespace is separate from the variable one, but the
            // resolution failure is the same class (the exit-3 compile error).
            Self::UndefinedEngineBinding { span, name } => write!(
                formatter,
                "{name} is not defined (no `as {name}` binder is in scope) at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            Self::EngineBindingAsValue { span, name } => write!(
                formatter,
                "{name} cannot be returned as a value; use `{name}.next` or `{name}.rest` \
                 at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            Self::EngineBindingProjection { span, name } => write!(
                formatter,
                "{name} has no such projection; the only engine-binding projections are \
                 `{name}.next` and `{name}.rest` at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            Self::UndefinedEngineConstructor { span, name } => write!(
                formatter,
                "{name} is not an engine constructor; the engine constructors are \
                 `~generator`, `~cursor`, `~inputs`, and `~rng` at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            Self::EngineBindingShape { span, reason } => {
                write!(formatter, "{reason} at bytes {}..{}", span.start(), span.end())
            }
            Self::EnginePullInRecursiveDef { span, name } => write!(
                formatter,
                "{name} cannot be pulled from inside a recursive `def` body (the \
                 carve-out); write the loop with `while`/`until`/`repeat`/`recurse` instead, \
                 at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            Self::EngineBindingInConstructor { span, name } => write!(
                formatter,
                "{name} cannot be captured by a `~generator` argument (a cursor's own \
                 machine cannot pull a cursor of the enclosing machine) at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            Self::EngineBindingLoopPattern { span } => write!(
                formatter,
                "an engine binding cannot be a `reduce`/`foreach` loop pattern; only \
                 `as ~x` introduces a cursor at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            Self::UndefinedLabel { span, name } => write!(
                formatter,
                "$*label-{} is not defined at bytes {}..{}",
                name.strip_prefix('$').unwrap_or(name),
                span.start(),
                span.end(),
            ),
            // Spelled exactly as the parser's own refusal and the resource
            // ledger's, because it is the same fact reached from a third place.
            Self::NestingTooDeep { span, limit } => write!(
                formatter,
                "nesting depth limit exceeded: the ceiling is {limit} levels, at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            // The resource error renders through its OWN Display — the
            // jqf-resource prose law: Debug on `ResourceError` is a
            // Rust struct literal (`LimitExceeded { limit_kind: MemoryBytes,
            // .. }`), and the CLI routes this message to exit 2, so a user
            // with a tiny `--max-memory-bytes` must read a sentence, not a
            // type.
            Self::Resource(error) => {
                write!(formatter, "cannot charge compiled program: {error}")
            }
        }
    }
}

impl core::error::Error for EngineCompileError {}

/// Parses and compiles one program into an opaque compiled arena.
///
/// The recognized surface is enumerated once, in [`SUPPORTED_SYNTAX`] and the
/// registry-generated [`RegisteredBuiltins`] — the same two clauses a user
/// reads in the rejection message, so this doc comment cannot drift from it.
/// The pipeline runs parse, lower, fuse, analyze, then charge. Every other
/// construct is rejected by name and span; the DYNAMIC `.@`/`.&` selector
/// forms get their own explicit rejections (the direct and quoted forms lower
/// to accessor steps).
///
/// # Errors
///
/// Returns [`EngineCompileError::Input`] when the source exceeds the syntax
/// input bound, [`EngineCompileError::Parse`] for any parser diagnostic,
/// [`EngineCompileError::Unsupported`] for a construct outside the subset, and
/// [`EngineCompileError::Resource`] when charging the compiled arena fails.
pub fn try_compile_program(
    source: &str,
    policy: CodecRequirementPolicy,
    resources: &ResourceContext<'_>,
) -> Result<CompiledProgram, EngineCompileError> {
    try_compile_program_with_args(source, policy, &[], resources)
}

/// Compiles the `--split-exp` program: the only compile entry that resolves
/// an unbound `$index` reference into a runtime variable slot (the
/// yq-compatible counter binding) instead of reporting `$index is not
/// defined`. The split drive seeds that slot per item with the item
/// counter; the returned program carries the slot so the drive knows which
/// one to seed (`CompiledProgram`'s `runtime_index_slot` field).
///
/// The split expression is compiled STANDALONE — it sees no `--arg` family
/// bindings (a `$name` reference inside it is the ordinary `$name is not
/// defined`). Only `$index` is pre-bound, which is the yq surface.
pub fn try_compile_program_split(
    source: &str,
    policy: CodecRequirementPolicy,
    resources: &ResourceContext<'_>,
) -> Result<CompiledProgram, EngineCompileError> {
    compile(source, policy, &[], resources, true)
}

/// Compiles one program in EDIT mode. Fact assignment lowers in every
/// compile, so this entry is behaviorally the ordinary compile; it remains
/// a distinct public entry so the CLI `--edit` lane keeps its own named
/// request surface.
pub fn try_compile_program_for_edit(
    source: &str,
    policy: CodecRequirementPolicy,
    resources: &ResourceContext<'_>,
) -> Result<CompiledProgram, EngineCompileError> {
    try_compile_program_for_edit_with_args(source, policy, &[], resources)
}

/// Compiles one program with `--arg`/`--argjson` CLI bindings in scope.
///
/// Each entry is `($name-with-prefix, value)`; later entries shadow earlier
/// ones (the last-wins law). A `$name` reference resolves against a lexical
/// binder FIRST (a program binder always beats a CLI binding), then against
/// this table, and only then reports `$x is not defined`. The value is lowered
/// to a literal producer at every reference site, which is what makes a CLI
/// binding a compile-time constant — `--arg` values are immutable, so
/// substitution is semantics-exact.
pub fn try_compile_program_with_args(
    source: &str,
    policy: CodecRequirementPolicy,
    args: &[(String, Value)],
    resources: &ResourceContext<'_>,
) -> Result<CompiledProgram, EngineCompileError> {
    compile(source, policy, args, resources, false)
}

/// Compiles one program in EDIT mode with `--arg`/`--argjson` bindings
/// in scope — the edit twin of [`try_compile_program_with_args`] (see
/// [`try_compile_program_for_edit`]).
pub fn try_compile_program_for_edit_with_args(
    source: &str,
    policy: CodecRequirementPolicy,
    args: &[(String, Value)],
    resources: &ResourceContext<'_>,
) -> Result<CompiledProgram, EngineCompileError> {
    compile(source, policy, args, resources, false)
}

#[allow(
    clippy::too_many_lines,
    reason = "the one compile entry keeps parse, gate, lower, fuse, analyze, and charge in one \
              linear sequence; splitting it would hide which stage a new rejection belongs to"
)]
fn compile(
    source: &str,
    policy: CodecRequirementPolicy,
    args: &[(String, Value)],
    resources: &ResourceContext<'_>,
    runtime_index: bool,
) -> Result<CompiledProgram, EngineCompileError> {
    let source_ref = SourceRef::new(SourceId::new(0), SourceKind::Query);
    // A source containing module DIRECTIVES (`import`, `include`, `module`)
    // still forces the prelude parse: a module file can call a prelude name
    // the top-level source never mentions. Every user program now parses as a
    // program unit (a plain query is a unit with no items). The prelude name
    // scan is a measured 3–10 % win on small inputs and is independent of
    // that parse choice.
    let uses_module_unit = ["import", "include", "module"]
        .iter()
        .any(|directive| source.contains(directive));
    // The preludes are parsed ONLY when the program could possibly call into
    // them.
    //
    // Parsing unconditionally cost 3-10 % on the small-input class — the
    // stdlib prelude is ~500 bytes of `def`s re-parsed per request, which is
    // invisible against a 10 MB document and is not invisible against a 7-byte
    // one. The gate is a substring scan over each prelude's names: a call to
    // `any` cannot occur in a program whose text never contains "any", so a
    // miss is sound. A false HIT (a path spelled `.allowed` contains "all")
    // only costs the parse we would otherwise always have paid. The extension
    // prelude rides the same gate with its own name list; its
    // common-word names (`lag`, `running`, `counter`) are the false-hit price.
    // A module file can call a prelude name the top-level source never
    // mentions (`include "m"; m_add(1; 2)`), so a program unit always pays
    // the prelude parse the module lowerer used to do itself.
    let needs_stdlib = uses_module_unit || STDLIB_NAMES.iter().any(|name| source.contains(name));
    let needs_extension = uses_module_unit || EXTENSION_NAMES.iter().any(|name| source.contains(name));
    let prelude_ref = SourceRef::new(SourceId::new(1), SourceKind::Query);
    let extension_ref = SourceRef::new(SourceId::new(2), SourceKind::Query);
    // `prelude_syntax` pairs are hoisted because the bound forms borrow them;
    // both must outlive lowering.
    let stdlib_syntax = if needs_stdlib {
        Some(
            parse_query(prelude_ref, STDLIB_PRELUDE)
                .map_err(|_| EngineCompileError::Parse(ParseRejection::internal("the stdlib prelude does not parse")))?
                .into_valid_syntax()
                .map_err(|_| EngineCompileError::Parse(ParseRejection::internal("the stdlib prelude is not valid")))?,
        )
    } else {
        None
    };
    let extension_syntax = if needs_extension {
        Some(
            parse_query(extension_ref, EXTENSION_PRELUDE)
                .map_err(|_| {
                    EngineCompileError::Parse(ParseRejection::internal("the extension prelude does not parse"))
                })?
                .into_valid_syntax()
                .map_err(|_| {
                    EngineCompileError::Parse(ParseRejection::internal("the extension prelude is not valid"))
                })?,
        )
    } else {
        None
    };
    let stdlib_bound = match stdlib_syntax.as_ref() {
        Some(syntax) => Some(
            syntax
                .bind(ResolvedSource::new(
                    prelude_ref,
                    "<stdlib>",
                    STDLIB_PRELUDE.as_bytes(),
                    0,
                ))
                .map_err(|_| EngineCompileError::Parse(ParseRejection::internal("the stdlib prelude does not bind")))?,
        ),
        None => None,
    };
    let extension_bound = match extension_syntax.as_ref() {
        Some(syntax) => Some(
            syntax
                .bind(ResolvedSource::new(
                    extension_ref,
                    "<extension>",
                    EXTENSION_PRELUDE.as_bytes(),
                    0,
                ))
                .map_err(|_| {
                    EngineCompileError::Parse(ParseRejection::internal("the extension prelude does not bind"))
                })?,
        ),
        None => None,
    };
    let preludes = [stdlib_bound.as_ref(), extension_bound.as_ref()]
        .into_iter()
        .flatten()
        .map(|bound| (bound.root(), bound.source()))
        .collect::<alloc::vec::Vec<_>>();
    let (mut lowered_nodes, lowered_root, slots, _engine_slots, callables, uses_inputs_cursor, runtime_index_slot) = {
        let parse = parse_program(source_ref, source).map_err(EngineCompileError::Input)?;
        let syntax = parse
            .into_valid_syntax()
            .map_err(|diagnostics| EngineCompileError::Parse(ParseRejection::from_diagnostics(&diagnostics)))?;
        let resolved = ResolvedSource::new(source_ref, "<top-level>", source.as_bytes(), 0);
        let bound = syntax
            .bind(resolved)
            .map_err(|error| EngineCompileError::Parse(ParseRejection::from_bind(error)))?;
        let mut prepared = alloc::vec::Vec::new();
        let mut seen = alloc::collections::BTreeSet::new();
        lower::prepare_included_modules(bound.root(), bound.source(), resources, None, &mut prepared, &mut seen)?;
        let module_bounds = prepared
            .iter()
            .map(|module| {
                module
                    .syntax
                    .bind(ResolvedSource::new(
                        module.syntax.source_ref(),
                        &module.label,
                        module.text.as_bytes(),
                        0,
                    ))
                    .map(|bound| lower::BoundModule {
                        label: module.label.as_str(),
                        dir: module.dir.as_str(),
                        bound,
                    })
            })
            .collect::<Result<alloc::vec::Vec<_>, _>>()
            .map_err(|error| EngineCompileError::Parse(ParseRejection::from_bind(error)))?;
        lower::lower_program_unit(
            &preludes,
            bound.root(),
            bound.source(),
            &module_bounds,
            args,
            resources,
            runtime_index,
        )?
    };
    // Fuse the pipe graph into the canonical single Stage, then split it exactly
    // as a pipe-free path: `.a | .b` and `.a.b` become the identical requirement.
    let (mut nodes, root) =
        fuse_with_callables(&mut lowered_nodes, lowered_root, &callables).map_err(EngineCompileError::Resource)?;
    let mut slots = slots;
    let root = rewrite_collect_add(&mut nodes, root, &mut slots)?;
    crate::analysis::mark_tail_calls(&mut nodes);
    crate::exec::GraphMachine::mark_keyed_collects(&mut nodes);
    crate::exec::mark_binary_shapes(&mut nodes);
    crate::exec::mark_static_object_keys(&mut nodes);
    let split = analyze(&nodes, root);
    let program = Program::new(nodes, root, split, slots);
    Ok(CompiledProgram::finish(
        program,
        policy,
        uses_inputs_cursor,
        runtime_index_slot,
    ))
}

/// `[STREAM] | add` → `reduce STREAM as $x (null; . + $x)`.
///
/// `add/0` folds the input array's children; collecting the stream only to
/// fold it is the peak the element/count rows already avoid. The rewrite
/// keeps `add`'s empty-input `null` and the native `+` update, so the
/// executor's ordinary reduce path serves it.
fn rewrite_collect_add(
    nodes: &mut Vec<ProgramNode>,
    root: ProgramNodeId,
    slots: &mut u32,
) -> Result<ProgramNodeId, EngineCompileError> {
    let Some(stream) = collect_add_stream(nodes, root) else {
        return Ok(root);
    };
    let slot = *slots;
    *slots = slot.checked_add(1).ok_or_else(|| {
        EngineCompileError::Parse(ParseRejection::internal(
            "program exceeds the variable slot addressing bound",
        ))
    })?;
    let init = push_rewrite_node(
        nodes,
        ProgramNode::Stage {
            start: StageStart::Literal(Value::Null),
            steps: Vec::new(),
        },
    )
    .ok_or(EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    let acc = push_rewrite_node(
        nodes,
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: Vec::new(),
        },
    )
    .ok_or(EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    let addend = push_rewrite_node(
        nodes,
        ProgramNode::Stage {
            start: StageStart::Variable(slot),
            steps: Vec::new(),
        },
    )
    .ok_or(EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    let update = push_rewrite_node(nodes, ProgramNode::binary(BinaryKind::Add, acc, addend))
        .ok_or(EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    push_rewrite_node(
        nodes,
        ProgramNode::Reduce {
            source: stream,
            slot,
            init,
            update,
            keyed_collect: None,
        },
    )
    .ok_or(EngineCompileError::Resource(ResourceError::AllocationFailed))
}

/// `[STREAM] | add`, or `PATH | (map(f) | add)` — pipe is right-associative, so
/// the latter is `FlatMap(PATH, FlatMap(Collect, add))`. The PATH spelling
/// prepends the static prefix onto the collect body in place so the reduce
/// source is `[PATH[] | f]`.
fn collect_add_stream(nodes: &mut [ProgramNode], root: ProgramNodeId) -> Option<ProgramNodeId> {
    let ProgramNode::FlatMap { upstream, body } = nodes[root.index()] else {
        return None;
    };
    if is_add_zero(nodes, body) {
        if let ProgramNode::CollectArray { body: Some(stream) } = nodes[upstream.index()] {
            return Some(stream);
        }
        return None;
    }
    let prefix = crate::analysis::path_steps::static_member_stage_steps(nodes, upstream)?;
    let ProgramNode::FlatMap {
        upstream: collect,
        body: add,
    } = nodes[body.index()]
    else {
        return None;
    };
    if !is_add_zero(nodes, add) {
        return None;
    }
    let ProgramNode::CollectArray { body: Some(stream) } = nodes[collect.index()] else {
        return None;
    };
    if !crate::analysis::path_steps::prepend_in_place(nodes, stream, &prefix).ok()? {
        return None;
    }
    Some(stream)
}

fn is_add_zero(nodes: &[ProgramNode], id: ProgramNodeId) -> bool {
    let ProgramNode::Call { overload, args, .. } = &nodes[id.index()] else {
        return false;
    };
    overload.get() == jqf_builtins::registry::builtins::id::ADD_ZERO && args.is_empty()
}

/// Appends one node to a rewrite arena, mirroring the lowerer's arena law:
/// the lowered-node ceiling and allocation-failure mapping (the
/// arena-level rewrite reuses the shared compile-time bound).
fn push_rewrite_node(nodes: &mut Vec<ProgramNode>, node: ProgramNode) -> Option<ProgramNodeId> {
    if nodes.len() >= 200_000 {
        return None;
    }
    let id = ProgramNodeId::from_index(nodes.len())?;
    nodes.try_reserve(1).ok()?;
    nodes.push(node);
    Some(id)
}

#[cfg(test)]
mod tests {
    use super::{
        CompiledProgram, EngineCompileError, UnsupportedConstruct, try_compile_program, try_compile_program_with_args,
    };
    use crate::analysis::BoundaryConsumer;
    use crate::codec_requirement::CodecRequirementPolicy;
    use crate::program::{ProgramNode, SliceBound, StageStart, StageStep, StepAccess};
    use alloc::{format, string::String, vec::Vec};
    use jqf_builtins::registry::Evaluator;
    use jqf_codec_core::{AccessRequirement, AccessResultKind, DiagnosticPolicy, PortableStep, ValidationMode};
    use jqf_data::Value;
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

    fn compile(source: &str) -> Result<CompiledProgram, EngineCompileError> {
        try_compile_program(source, policy(), &resources())
    }

    #[test]
    fn slurp_aggregate_rewrite_recognizes_the_map_shapes() {
        use crate::program::ProgramNode;
        // add and length, fused-stage and flatmap map bodies, and the explicit
        // `[.[] | F] | add` spelling (which lowers identically to `map(F)|add`).
        for src in [
            "map(.a)|add",
            "map(.a)|length",
            "map(select(.a>1))|add",
            "map(.[0])|length",
            "[.[]|.a]|add",
            "map(.a?)|add",
        ] {
            let rewritten = compiled(src)
                .streaming_slurp_aggregate()
                .unwrap_or_else(|| panic!("{src} must be recognized"));
            match &rewritten.arena()[rewritten.root().index()] {
                ProgramNode::Reduce { .. } => {}
                other => panic!("{src}: expected a Reduce root, got {other:?}"),
            }
        }
        // Every other shape declines: non-map pipes, arity-1 aggregates,
        // nothing after the aggregate, a bare map, an identity pipe.
        for src in [
            ".a",
            "map(.a)|add|length",
            "map(.a)",
            "add",
            "length",
            "map(.a)|.",
            ".[]|add",
        ] {
            assert!(
                compiled(src).streaming_slurp_aggregate().is_none(),
                "{src} must decline"
            );
        }
    }

    #[test]
    fn collect_add_rewrites_to_reduce() {
        for src in [
            "[.catalog[] | .score] | add",
            ".catalog | map(.score) | add",
            "map(.score) | add",
        ] {
            let program = compiled(src);
            match &program.arena()[program.root().index()] {
                ProgramNode::Reduce { .. } => {}
                other => panic!("{src}: expected Reduce, got {other:?}"),
            }
        }
    }

    fn compiled(source: &str) -> CompiledProgram {
        compile(source).expect("compiles")
    }

    /// A stored Call carries the evaluator payload resolved at lower time.
    #[test]
    fn call_nodes_store_the_evaluator_payload() {
        assert!(
            compiled("length").arena().iter().any(|node| {
                matches!(
                    node,
                    ProgramNode::Call {
                        payload: Evaluator::Length,
                        ..
                    }
                )
            }),
            "length must store Evaluator::Length on the Call node"
        );
    }

    fn compiled_with_args(source: &str, args: &[(String, Value)]) -> CompiledProgram {
        try_compile_program_with_args(source, policy(), args, &resources()).expect("compiles")
    }

    /// The loss-lane programs and the exact prune trees the walk must hand the
    /// decode — the guard net over the fold, bind, and union shapes.
    #[test]
    fn prune_trees_over_the_loss_lane_shapes() {
        fn render(node: &crate::analysis::PruneNode, out: &mut String) {
            if node.is_all() {
                out.push('*');
                return;
            }
            out.push('{');
            if let Some(element) = node.element() {
                out.push_str("[]:");
                render(element, out);
            }
            for (key, child) in node.keys() {
                out.push(',');
                out.push_str(key);
                out.push(':');
                render(child, out);
            }
            out.push('}');
        }
        let lanes: &[(&str, Option<&str>)] = &[
            (
                "[[.users[].id], [.users[].name], [.users[].age]] | transpose | length",
                Some("{,users:{[]:{,age:*,id:*,name:*}}}"),
            ),
            (
                "[foreach .users[] as $u (0; . + 1; {n: ., id: $u.id})] | length",
                Some("{,users:{[]:{,id:*}}}"),
            ),
            (
                "[foreach .users[] as $u ({}; .[$u.tier] = true; length)] | last",
                Some("{,users:{[]:{,tier:*}}}"),
            ),
            (
                "reduce .events[] as $e ({}; .[$e.bucket] = ((.[$e.bucket] // 0) + 1))",
                Some("{,events:{[]:{,bucket:*}}}"),
            ),
            (
                "reduce .orders[] as [$user_id, $quantity] (0; . + $quantity)",
                Some("{,orders:{[]:*}}"),
            ),
        ];
        for (source, expected) in lanes {
            let tree = compile(source).expect("compiles").prune_tree().cloned();
            let rendered = tree.map(|tree| {
                let mut out = String::new();
                render(&tree, &mut out);
                out
            });
            assert_eq!(rendered.as_deref(), *expected, "{source}");
        }
    }

    #[test]
    fn cli_args_bind_unbound_variables_to_literals() {
        let args = [
            (
                alloc::string::String::from("$name"),
                Value::try_string("hello").expect("string"),
            ),
            (
                alloc::string::String::from("$n"),
                Value::try_string("2").expect("string"),
            ),
        ];
        compiled_with_args("$name", &args);
        compiled_with_args("$name, $n", &args);
    }

    #[test]
    fn cli_args_do_not_shadow_lexical_binders_or_env() {
        let args = [
            (
                alloc::string::String::from("$x"),
                Value::try_string("cli").expect("string"),
            ),
            (
                alloc::string::String::from("$ENV"),
                Value::try_string("cli").expect("string"),
            ),
        ];
        // A program binder wins: `1 as $x | $x` must be the binder, not the arg.
        compiled_with_args("1 as $x | $x", &args);
        // `$ENV` stays the environment read (`--arg ENV x` is shadowed by the
        // pre-bound environment binding).
        compiled_with_args("$ENV", &args);
        // A name bound by NEITHER the scopes nor the args is still the error.
        assert!(matches!(
            compile_with_args_err("$missing", &args),
            Err(EngineCompileError::UndefinedVariable { .. })
        ));
    }

    /// `{$ENV}`, `.[$ENV]` and `.[$ENV:]` compile. The three direct
    /// `resolve_variable` sites raised where the lowered path had already
    /// lowered `$ENV` to the `env/0` read; the reference accepts all three
    /// spellings, and each now routes `$ENV` through the same
    /// env/0 lowering — a member's value/key producer, an index operand
    /// bound into a frame, and a slice bound. The runtime outcomes are
    /// pinned by the corpus rows; this test pins the acceptance.
    #[test]
    fn env_variable_spellings_compile() {
        compiled("{$ENV}");
        compiled("{$ENV: 1}");
        compiled(".[$ENV]");
        compiled(".[$ENV:]");
        compiled(".[:$ENV]");
        // The binding is still not an ordinary scope slot: a BINDER named
        // `$ENV` shadows it lexically.
        compiled("1 as $ENV | $ENV");
    }

    /// A CLI binding serves a variable in OPERAND position exactly as it
    /// serves expression position: `.[$k]`, a slice bound, and a dynamic
    /// accessor selector all lower through the general operand path under
    /// `--arg`, while a genuinely unbound name still reports the ordinary
    /// `$x is not defined` resolution error.
    #[test]
    fn cli_args_serve_operand_position_variables() {
        let args = [(
            alloc::string::String::from("$k"),
            Value::try_string("key").expect("string"),
        )];
        compiled_with_args(".[$k]", &args);
        compiled_with_args(".a[$k:] | length", &args);
        compiled_with_args(".[:$k]", &args);
        compiled_with_args(".@($k)", &args);
        compiled_with_args(".&($k)", &args);
        assert!(matches!(
            compile_with_args_err(".[$nope]", &args),
            Err(EngineCompileError::UndefinedVariable { .. })
        ));
    }

    /// The named bindings serve operand positions through the same expression
    /// ladder as everywhere else: `$__loc__` compiles at an accessor and
    /// raises its ordinary runtime mismatch there (`{file,line}` is not a
    /// fact name), and `$ENV` lowers to the env/0 read.
    #[test]
    fn named_operand_bindings_keep_their_surface() {
        compiled(".[$__loc__]");
        compiled(".@($__loc__)");
        compiled(".[$ENV]");
        compiled(".@($ENV)");
        // The location literal itself is unchanged: the operand frame carries
        // exactly the value expression position produces.
        let compiled = compiled(".[$__loc__]");
        assert!(compiled.arena().iter().any(|node| matches!(
            node,
            crate::program::ProgramNode::Stage {
                start: crate::program::StageStart::Literal(_),
                ..
            }
        )));
    }

    /// A call with more arguments than the registry's one-byte arity ceiling
    /// is rejected naming the AUTHORED count — never a clamped
    /// `name/255 is not defined` reporting an arity the user never wrote.
    #[test]
    fn an_arity_past_the_ceiling_names_the_authored_count() {
        let mut source = alloc::string::String::from("f(");
        for i in 0..300 {
            if i > 0 {
                source.push_str("; ");
            }
            source.push('1');
        }
        source.push(')');
        match compile_with_args_err(&source, &[]) {
            Err(EngineCompileError::ArityLimit { count, .. }) => assert_eq!(count, 300),
            other => panic!("expected the arity-limit rejection, got {other:?}"),
        }
    }

    #[test]
    fn cli_args_inside_defs_close_over_the_literal() {
        let args = [(
            alloc::string::String::from("$v"),
            Value::try_string("V").expect("string"),
        )];
        // `def f: $v; f` with `--arg v V` answers "V".
        compiled_with_args("def f: $v; f", &args);
    }

    #[test]
    fn later_cli_args_shadow_earlier_ones() {
        let resources = resources();
        let args = [
            (
                alloc::string::String::from("$a"),
                Value::try_string("first").expect("string"),
            ),
            (
                alloc::string::String::from("$a"),
                Value::try_string("second").expect("string"),
            ),
        ];
        // The last-wins law: `--arg a 1 --arg a 2` answers "2".
        let program = try_compile_program_with_args("$a", policy(), &args, &resources).expect("compiles");
        let nodes = program.program.nodes();
        let literals: Vec<_> = nodes
            .iter()
            .filter_map(|node| match node {
                ProgramNode::Stage {
                    start: crate::program::StageStart::Literal(value),
                    ..
                } => Some(match value.untagged() {
                    jqf_data::Value::String(text) => text.as_str(),
                    _ => "?",
                }),
                _ => None,
            })
            .collect();
        assert_eq!(literals, ["second"], "the later binding must win");
    }

    fn compile_with_args_err(source: &str, args: &[(String, Value)]) -> Result<CompiledProgram, EngineCompileError> {
        try_compile_program_with_args(source, policy(), args, &resources())
    }

    #[test]
    fn is_identity_accepts_only_the_bare_identity_filter() {
        assert!(compiled(".").is_identity(), "bare `.` must be recognized as identity");
        // `. | .` lowers through stage fusion to the same zero-step `Current`
        // stage, so it is provably identity too: it emits its input once,
        // unchanged, for every input.
        assert!(
            compiled(". | .").is_identity(),
            "a fused identity pipe must be recognized as identity"
        );
        // The round-trip lane publishes the input bytes UNCHANGED, so anything
        // that could emit a different value or cardinality than its input —
        // every static path, iteration, recursion, constructor, call, and pipe
        // shape — must be rejected.
        for source in [
            ".a",
            ".a.b",
            ".a[0]",
            ".[]",
            "..",
            "[.]",
            ".a, .b",
            "(.a)",
            ".[] | .",
            "empty",
            "length",
            "input",
            "select(.)",
            "try .",
        ] {
            assert!(
                !compiled(source).is_identity(),
                "{source:?} must not be recognized as identity"
            );
        }
    }

    #[test]
    fn modifies_answers_whether_the_program_assigns() {
        for source in [".", ".a", ".[]", "length", "select(.)"] {
            assert!(
                !compiled(source).modifies(),
                "{source:?} must not contain an assignment"
            );
        }
        for source in [".a = 1", ".a |= . + 1", "del(.a)", ".a += 1", "map_values(.)"] {
            assert!(compiled(source).modifies(), "{source:?} must contain an assignment");
        }
    }

    /// The steps of a program whose root is a single [`Stage`] (identity, static
    /// path, iteration, or pipe fused to one stage) — the shape these tests
    /// inspect. Panics for a graph root, which these tests never build.
    ///
    /// [`Stage`]: ProgramNode::Stage
    fn root_stage_steps(program: &CompiledProgram) -> &[StageStep] {
        match &program.program.nodes()[program.program.root().index()] {
            ProgramNode::Stage { steps, .. } => steps,
            other => panic!("expected a Stage root, got {other:?}"),
        }
    }

    fn keys(program: &CompiledProgram) -> Vec<&str> {
        root_stage_steps(program)
            .iter()
            .filter_map(|step| match step.access() {
                StepAccess::Key(key) => Some(key.as_str()),
                StepAccess::Index(_)
                | StepAccess::Each
                | StepAccess::Descend
                | StepAccess::Slice(_)
                | StepAccess::DynVar(_)
                | StepAccess::NodeAccessor(_)
                | StepAccess::Attribute(_)
                | StepAccess::DynNodeAccessor(_)
                | StepAccess::DynAttribute(_) => None,
            })
            .collect()
    }

    fn indices(program: &CompiledProgram) -> Vec<i64> {
        root_stage_steps(program)
            .iter()
            .filter_map(|step| match step.access() {
                StepAccess::Index(index) => Some(*index),
                StepAccess::Key(_)
                | StepAccess::Each
                | StepAccess::Descend
                | StepAccess::Slice(_)
                | StepAccess::DynVar(_)
                | StepAccess::NodeAccessor(_)
                | StepAccess::Attribute(_)
                | StepAccess::DynNodeAccessor(_)
                | StepAccess::DynAttribute(_) => None,
            })
            .collect()
    }

    /// The per-component optional (`?`) flags of the fused root stage, in order.
    fn optional_flags(program: &CompiledProgram) -> Vec<bool> {
        root_stage_steps(program).iter().map(StageStep::is_optional).collect()
    }

    fn requirement_of(source: &str) -> AccessRequirement {
        let resources = resources();
        compiled(source).try_requirement(&resources).expect("requirement")
    }

    fn unsupported(source: &str) -> UnsupportedConstruct {
        match compile(source) {
            Err(EngineCompileError::Unsupported { construct, .. }) => construct,
            other => panic!("expected unsupported construct for {source:?}, got {other:?}"),
        }
    }

    /// The full rejection message as a user reads it.
    fn unsupported_error(source: &str) -> EngineCompileError {
        match compile(source) {
            Err(error @ EngineCompileError::Unsupported { .. }) => error,
            other => panic!("expected an unsupported rejection for {source:?}, got {other:?}"),
        }
    }

    /// The rejection message as a user reads it, for the two honesty guards
    /// below.
    fn rejection_message() -> String {
        match compile("module (.+1); 0") {
            Err(error @ EngineCompileError::Unsupported { .. }) => alloc::format!("{error}"),
            other => panic!("expected an unsupported rejection, got {other:?}"),
        }
    }

    /// The STALE-MESSAGE TRIPWIRE for the builtin half.
    ///
    /// The clause is generated from the registry, so this cannot fail while the
    /// generation stands; what it pins is that the generation stands. Anyone who
    /// replaces the generated clause with hand-written prose — which is how the
    /// message went stale the first time, claiming four builtins when seven
    /// resolved — fails here the moment the registry gains an overload the prose
    /// does not mention.
    #[test]
    fn the_message_names_every_registered_builtin() {
        let message = rejection_message();
        assert!(!jqf_builtins::registry::builtin_overloads().is_empty());
        for overload in jqf_builtins::registry::builtin_overloads() {
            let spelling = alloc::format!("`{}/{}`", overload.canonical_name, overload.arity);
            assert!(
                message.contains(&spelling),
                "rejection message does not name the registered builtin {spelling}: {message}"
            );
        }
    }

    /// The mirror guard for the SYNTAX half: every form the message advertises
    /// must actually compile.
    ///
    /// The message's job is to tell a user what jqf runs. A claim that is no
    /// longer true is the same defect as an omission, and the syntax clause has
    /// no registry to generate it from — so each named form gets one probe
    /// program here. Widening the grammar means adding the form to
    /// `SUPPORTED_SYNTAX` and its probe to this list.
    #[test]
    fn the_message_names_only_forms_that_compile() {
        let probes = [
            (".", "identity `.`"),
            (".a", "field path"),
            (".\"k\"", "quoted field path"),
            (".[\"k\"]", "bracketed string key"),
            (".[0]", "index"),
            (".[-1]", "negative index"),
            (".a?", "per-component `?`"),
            (".[]", "`.[]` iteration"),
            ("..", "recursive descent `..`"),
            (".[1:2]", "literal-bound slice"),
            ("1 as $x | .[$x:]", "`$var`-bound slice"),
            (".[.a:.b]", "general-bound slice"),
            ("1 as $x | .[$x]", "dynamic indexing by `$var`"),
            (".[.k]", "dynamic indexing `.[e]`"),
            ("-.a", "unary negation `-`"),
            (".a | .b", "pipe `|`"),
            (".a, .b", "comma `,`"),
            ("(.a)", "parenthesized group"),
            ("1", "literal"),
            ("[.]", "`[…]` constructor"),
            ("{a: 1}", "`{…}` constructor"),
            (". + 1", "arithmetic `+`"),
            (". - 1", "arithmetic `-`"),
            (". * 1", "arithmetic `*`"),
            (". / 1", "arithmetic `/`"),
            (". % 1", "arithmetic `%`"),
            (". == 1", "comparison `==`"),
            (". != 1", "comparison `!=`"),
            (". < 1", "comparison `<`"),
            (". <= 1", "comparison `<=`"),
            (". > 1", "comparison `>`"),
            (". >= 1", "comparison `>=`"),
            (". and .", "`and`"),
            (". or .", "`or`"),
            (". // 1", "the alternative `//`"),
            ("if . then 1 else 2 end", "`if`/`else`"),
            ("if . then 1 elif . then 2 else 3 end", "`elif`"),
            ("try .a catch .", "`try`/`catch`"),
            ("(.a)?", "the `?` sugar"),
            (". as $x | $x", "`as` binding over a variable"),
            (". as [$a,$b] | $a", "`as [$a,$b]` array pattern"),
            (". as {a:$v,$b} | $v", "`as {a:$v,$b}` object pattern"),
            (". as [$a] ?// {$a} | $a", "the `?//` alternative chain"),
            ("reduce .[] as $x (0; . + $x)", "`reduce`"),
            ("foreach .[] as $x (0; . + $x)", "`foreach`"),
            ("label $out | break $out", "`label $out`/`break $out`"),
            (".a = 1", "assignment `=`"),
            (".a |= . + 1", "update `|=`"),
            (".a += 1", "the arithmetic update `+=`"),
            (".a -= 1", "the arithmetic update `-=`"),
            (".a *= 1", "the arithmetic update `*=`"),
            (".a /= 1", "the arithmetic update `/=`"),
            (".a %= 1", "the arithmetic update `%=`"),
            (".a //= 1", "the alternative update `//=`"),
        ];
        for (source, claim) in probes {
            assert!(
                compile(source).is_ok(),
                "the rejection message advertises {claim}, but {source:?} does not compile"
            );
        }
        // Every claim above must also be one the message actually makes; a
        // probe for a form the message stopped advertising is dead weight.
        let message = rejection_message();
        for fragment in [
            "identity `.`",
            "`.[]` iteration",
            "recursive descent `..`",
            "dynamic indexing `.[e]`",
            "unary negation `-`",
            "pipe `|`",
            "comma `,`",
            "arithmetic `+ - * / %`",
            "comparisons `== != < <= > >=`",
            "`and`/`or`",
            "the alternative `//`",
            "`if`/`elif`/`else`",
            "`try`/`catch`",
            "`as` bindings over a variable or a destructuring pattern",
            "its `?//` alternative chain",
            "`reduce`",
            "`foreach`",
            "assignment `=`",
            "update `|=`",
            "`+= -= *= /= %= //=`",
        ] {
            assert!(
                message.contains(fragment),
                "the supported-surface enumeration no longer states {fragment:?}: {message}"
            );
        }
    }

    #[test]
    fn identity_is_whole_document_access() {
        let program = compiled(".");
        assert!(program.program.split().is_whole_document());
        assert!(root_stage_steps(&program).is_empty());
    }

    #[test]
    fn identity_requirement_is_complete_document() {
        let resources = resources();
        let program = compile(".").expect("identity");
        let requirement = program.try_requirement(&resources).expect("requirement");
        assert_eq!(requirement.result(), AccessResultKind::CompleteDocument);
    }

    #[test]
    fn single_field_path() {
        assert_eq!(keys(&compiled(".a")), ["a"]);
    }

    #[test]
    fn nested_field_path() {
        assert_eq!(keys(&compiled(".a.b")), ["a", "b"]);
    }

    #[test]
    fn quoted_field_key_with_spaces() {
        assert_eq!(keys(&compiled(r#"."quoted key""#)), ["quoted key"]);
    }

    #[test]
    fn bracketed_string_key() {
        assert_eq!(keys(&compiled(r#".["a"]"#)), ["a"]);
    }

    #[test]
    fn unicode_escape_key_decodes() {
        assert_eq!(keys(&compiled(r#"."café""#)), ["caf\u{e9}"]);
    }

    #[test]
    fn field_then_index() {
        let program = compiled(".a[0]");
        assert_eq!(keys(&program), ["a"]);
        assert_eq!(indices(&program), [0]);
    }

    #[test]
    fn bare_positive_index() {
        assert_eq!(indices(&compiled(".[2]")), [2]);
    }

    #[test]
    fn negative_index_preserves_sign() {
        assert_eq!(indices(&compiled(".[-1]")), [-1]);
    }

    #[test]
    fn deep_mixed_chain() {
        let program = compiled(r#".a.b[0]["c"][-2]"#);
        assert_eq!(keys(&program), ["a", "b", "c"]);
        assert_eq!(indices(&program), [0, -2]);
    }

    #[test]
    fn forward_requirement_preserves_members_and_signed_indices() {
        let resources = resources();
        let program = compile(".items[-1]").expect("path");
        let requirement = program.try_requirement(&resources).expect("requirement");
        let steps = requirement.footprint().exact_path().expect("exact").steps();
        assert!(matches!(&steps[0], PortableStep::SemanticMember(m) if m == "items"));
        assert!(matches!(steps[1], PortableStep::SemanticIndex(-1)));
    }

    #[test]
    fn forward_requirement_reanchors_kept_subtree_onto_the_located_node() {
        // Static prefix `.catalog`, residual `{id,name}`: the located
        // requirement carries a prune naming those two keys.
        let program = compiled(".catalog | {id,name}");
        assert_eq!(
            program.reanchor_prune_keys().as_deref(),
            Some([String::from("id"), String::from("name")].as_slice())
        );
        let resources = resources();
        let requirement = program.try_requirement(&resources).expect("requirement");
        assert_eq!(requirement.result(), AccessResultKind::Located);
        let steps = requirement.footprint().exact_path().expect("exact").steps();
        assert_eq!(steps.len(), 1);
        assert!(matches!(&steps[0], PortableStep::SemanticMember(m) if m == "catalog"));
        let tree = requirement.prune().expect("prune attached");
        let names: Vec<&str> = tree.root().members().map(|(name, _)| name).collect();
        assert_eq!(names, ["id", "name"]);
        assert!(
            tree.root().element().is_none(),
            "the located catalog object is not an iterated element"
        );

        // `.catalog[] | .name`: the path crosses an element. Re-anchoring
        // declines — the hint would not be rooted at the pushed-down node.
        let iterated = compiled(".catalog[] | .name");
        assert!(
            iterated.reanchor_prune_keys().is_none(),
            "re-anchor must decline when the residual starts at `.[]`"
        );
    }

    #[test]
    fn identity_lowers_through_root_requirement() {
        // Identity must reach whole-document access, never a forward requirement
        // with an empty path (which would be an exact-root located selection).
        let requirement = requirement_of(".");
        assert_eq!(requirement.result(), AccessResultKind::CompleteDocument);
        assert!(requirement.footprint().is_whole());
    }

    #[test]
    fn a_piped_identity_does_not_drag_the_whole_document_in() {
        // `.` is `|`'s unit, and the lowering folds it out — which matters
        // because a bare identity stage IS a whole-document requirement (the
        // test above). Left on the spine it defeated every downstream
        // projection: `. | .users | length` decoded a 9 MB fixture in 77ms
        // where `.users | length` decoded one member in 36ms.
        for source in [". | .a.b", ".a.b | .", "(.) | .a.b", ". | . | .a.b"] {
            let requirement = requirement_of(source);
            assert_eq!(
                requirement.result(),
                AccessResultKind::Located,
                "{source} must project like the bare path"
            );
            assert!(!requirement.footprint().is_whole(), "{source}");
        }
    }

    #[test]
    fn the_corpus_floor_forcing_wrapper_takes_the_whole_document_route() {
        // The floor is forced by wrapping the authored filter as
        // `[.][0] | (P)` — the spelling the `floorparity` rows and the sdk-smoke
        // projection-vs-floor oracle both use. Those comparisons are only worth
        // something if the wrapper genuinely defeats the pushdown, which is
        // exactly what this pins: a constructor on the upstream spine pushes
        // NOTHING down, so the wrapped requirement is whole-document while the
        // bare filter's is a located exact path.
        for (bare_source, floor_source) in [
            (".a.b", "[.][0] | (.a.b)"),
            (".catalog[0].id", "[.][0] | (.catalog[0].id)"),
        ] {
            let bare = requirement_of(bare_source);
            assert_eq!(
                bare.result(),
                AccessResultKind::Located,
                "{bare_source} must take a located route unwrapped"
            );
            assert!(!bare.footprint().is_whole());

            let floor = requirement_of(floor_source);
            assert_eq!(
                floor.result(),
                AccessResultKind::CompleteDocument,
                "{floor_source} must take the whole-document floor"
            );
            assert!(floor.footprint().is_whole());
        }
        // `.catalog[].name` is an ELEMENT row: it lowers the
        // whole-document requirement with the element demand hint, exactly
        // like a count row — the codec's span skeleton must survive for the
        // document-core consumer to iterate it. The wrapper keeps its own
        // whole-document floor, so the two agree on the route.
        let bare = requirement_of(".catalog[].name");
        assert_eq!(
            bare.result(),
            AccessResultKind::CompleteDocument,
            "the fan-out row must take the whole-document route"
        );
        assert!(bare.footprint().is_whole());
        assert!(
            bare.element().is_some(),
            "the fan-out row must carry the element demand hint"
        );
        let floor = requirement_of("[.][0] | (.catalog[].name)");
        assert_eq!(
            floor.result(),
            AccessResultKind::CompleteDocument,
            "the wrapper must take the whole-document floor"
        );
        assert!(floor.footprint().is_whole());
    }

    #[test]
    fn nested_keys_lower_to_exact_members() {
        let requirement = requirement_of(".a.b");
        assert_eq!(requirement.result(), AccessResultKind::Located);
        let steps = requirement.footprint().exact_path().expect("exact").steps();
        assert_eq!(steps.len(), 2);
        assert!(matches!(&steps[0], PortableStep::SemanticMember(m) if m == "a"));
        assert!(matches!(&steps[1], PortableStep::SemanticMember(m) if m == "b"));
    }

    #[test]
    fn quoted_and_unicode_keys_lower_decoded() {
        let quoted = requirement_of(r#"."quoted key""#);
        let steps = quoted.footprint().exact_path().expect("exact").steps();
        assert_eq!(steps.len(), 1);
        assert!(matches!(&steps[0], PortableStep::SemanticMember(m) if m == "quoted key"));

        let unicode = requirement_of(r#"."café""#);
        let steps = unicode.footprint().exact_path().expect("exact").steps();
        assert_eq!(steps.len(), 1);
        assert!(matches!(&steps[0], PortableStep::SemanticMember(m) if m == "caf\u{e9}"));
    }

    #[test]
    fn bracket_string_key_lowers_to_member() {
        let requirement = requirement_of(r#".["a"]"#);
        let steps = requirement.footprint().exact_path().expect("exact").steps();
        assert_eq!(steps.len(), 1);
        assert!(matches!(&steps[0], PortableStep::SemanticMember(m) if m == "a"));
    }

    #[test]
    fn positive_and_negative_indices_preserve_sign() {
        let positive = requirement_of(".[2]");
        let steps = positive.footprint().exact_path().expect("exact").steps();
        assert!(matches!(steps[0], PortableStep::SemanticIndex(2)));

        let negative = requirement_of(".[-1]");
        let steps = negative.footprint().exact_path().expect("exact").steps();
        assert!(matches!(steps[0], PortableStep::SemanticIndex(-1)));
    }

    #[test]
    fn deep_mixed_chain_lowers_in_order() {
        let requirement = requirement_of(r#".a.b[0]["c"][-2]"#);
        assert_eq!(requirement.result(), AccessResultKind::Located);
        let steps = requirement.footprint().exact_path().expect("exact").steps();
        assert_eq!(steps.len(), 5);
        assert!(matches!(&steps[0], PortableStep::SemanticMember(m) if m == "a"));
        assert!(matches!(&steps[1], PortableStep::SemanticMember(m) if m == "b"));
        assert!(matches!(steps[2], PortableStep::SemanticIndex(0)));
        assert!(matches!(&steps[3], PortableStep::SemanticMember(m) if m == "c"));
        assert!(matches!(steps[4], PortableStep::SemanticIndex(-2)));
    }

    #[test]
    fn dynamic_string_selector_folds_to_a_static_accessor_step() {
        // A hole-free string in `.@(…)` / `.&(…)` is the same compile-time
        // name the direct form carries — the `.[expr]` constant-fold arm.
        let node = compiled(r#".@("tag")"#);
        assert!(matches!(
            root_stage_steps(&node)[0].access(),
            StepAccess::NodeAccessor(name) if name == "tag"
        ));
        let alias = compiled(r#".@("comment_head")"#);
        assert!(matches!(
            root_stage_steps(&alias)[0].access(),
            StepAccess::NodeAccessor(name) if name == "comment"
        ));
        let attribute = compiled(r#".&("href")"#);
        assert!(matches!(
            root_stage_steps(&attribute)[0].access(),
            StepAccess::Attribute(name) if name == "href"
        ));
    }

    #[test]
    fn dynamic_variable_selector_lowers_to_a_dyn_accessor_step() {
        // A bare `$name` is the slot itself, not a `DynVar` key/index step.
        let node = compiled(r#""tag" as $name | .@($name)"#);
        let nodes = node.program.nodes();
        let ProgramNode::Bind { slot, body, .. } = &nodes[node.program.root().index()] else {
            panic!("a bound selector must lower to a binder frame");
        };
        let ProgramNode::Stage { steps, .. } = &nodes[body.index()] else {
            panic!("the frame must wrap the accessor stage");
        };
        assert!(matches!(
            steps[0].access(),
            StepAccess::DynNodeAccessor(bound) if bound == slot
        ));

        let attribute = compiled(r#""href" as $attr | .&($attr)"#);
        let nodes = attribute.program.nodes();
        let ProgramNode::Bind { slot, body, .. } = &nodes[attribute.program.root().index()] else {
            panic!("a bound attribute selector must lower to a binder frame");
        };
        let ProgramNode::Stage { steps, .. } = &nodes[body.index()] else {
            panic!("the frame must wrap the attribute stage");
        };
        assert!(matches!(
            steps[0].access(),
            StepAccess::DynAttribute(bound) if bound == slot
        ));
    }

    #[test]
    fn compiled_dynamic_tag_read_matches_static() {
        use crate::exec::{EngineRun, RunPoll};
        use jqf_builtins::codec_result::{CodecInputOutcome, EngineResult};
        let mut resources = resources();
        let tagged = Value::try_tagged(
            jqf_data::TagId::try_new_unaccounted("!money").expect("tag"),
            Value::Null,
        )
        .expect("tagged");
        let mut collect = |program: &str| {
            let compiled = compiled(program);
            let mut stream = match compiled
                .try_run_whole_value(
                    CodecInputOutcome::Result(EngineResult::owned(tagged.clone())),
                    &resources,
                )
                .expect("run")
            {
                EngineRun::Stream { stream, .. } => stream,
                other => panic!("{program}: {other:?}"),
            };
            match stream.poll(&mut resources) {
                Ok(RunPoll::Item(EngineResult::Owned(Value::String(text)))) => String::from(text.as_str()),
                other => panic!("{program}: {other:?}"),
            }
        };
        assert_eq!(collect(".@tag"), "!money");
        assert_eq!(collect(r#".@("tag")"#), "!money");
        assert_eq!(collect(r#""tag" as $name | .@($name)"#), "!money");
    }

    #[test]
    fn user_empty_shadows_the_syntax_form_and_literals_do_not() {
        // A user `empty/0` shadows the syntax form; bare `true`/`false`/`null`
        // stay literals even when a matching def exists. `true(5)` is a call.
        use crate::exec::{EngineRun, RunPoll};
        use jqf_builtins::codec_result::{CodecInputOutcome, EngineResult};
        let mut resources = resources();
        let mut collect = |program: &str| -> Vec<String> {
            let compiled = compiled(program);
            let mut stream = match compiled
                .try_run_whole_value(CodecInputOutcome::Result(EngineResult::owned(Value::Null)), &resources)
                .expect("run")
            {
                EngineRun::Stream { stream, .. } => stream,
                other => panic!("{program}: {other:?}"),
            };
            let mut out = Vec::new();
            loop {
                match stream.poll(&mut resources) {
                    Ok(RunPoll::Item(EngineResult::Owned(value))) => {
                        out.push(jqf_builtins::semantics::render::to_json(&value).expect("json"));
                    }
                    Ok(RunPoll::Complete) => break,
                    other => panic!("{program}: {other:?}"),
                }
            }
            out
        };
        assert_eq!(collect("def empty: 1; empty"), ["1"]);
        assert_eq!(collect("def true: 1; true"), ["true"]);
        assert_eq!(collect("def true($x): $x; true(5)"), ["5"]);
        assert_eq!(
            collect("def empty($x): $x; empty"),
            Vec::<String>::new(),
            "bare empty with only empty/1 in scope is still the syntax form"
        );
    }

    #[test]
    fn dynamic_selector_writes_compile_to_fact_assigns() {
        // A dynamic selector on a WRITE lowers like its static
        // twin — a hole-free string folds STATIC (`.@("comment")` ≡ `.@comment`),
        // and a real expression binds outside the write for runtime validation
        // against the closed vocabulary. Lane 2 rides beside it: computed-base
        // targets (`1 as $i | .[$i].@comment`) lower through the same node.
        for program in [
            r#"."c" as $r | .@($r) = "x""#,
            r#".@("tag") = "x""#,
            r#".@("comment") = ["x"]"#,
            r#"."h" as $h | .&($h) |= "u""#,
            r#".&("href") |= "u""#,
            r#"."k" as $k | .[$k].@comment = ["x"]"#,
            r#"1 as $i | .[$i].&href = "u""#,
        ] {
            let compiled = compiled(program);
            assert!(compiled.fact_writes(), "{program} must lower to a FactAssign");
        }
    }

    #[test]
    fn accessor_writes_that_are_not_the_last_step_stay_rejected() {
        // Nested/dotted fact paths are rejected: the role
        // vocabulary is flat, so `.name.@comment.leading` names no role. An
        // accessor left INSIDE the prefix fails the key/index-chain check.
        assert_eq!(
            unsupported(r#".name.@comment.leading = "x""#),
            UnsupportedConstruct::AccessorAssignment
        );
        match unsupported(r#".a.@tag.@comment = "x""#) {
            UnsupportedConstruct::Expression(message) => {
                assert!(message.contains("non-static path"), "{message}");
            }
            other => panic!("expected the path-shape rejection, got {other:?}"),
        }
    }

    #[test]
    fn rejects_assignment_over_accessor_paths() {
        // A non-admitted selector like `.@bogus` stays the deferred-write
        // rejection. Admitted fact targets (`.@comment`, `.&href`) compile.
        assert_eq!(
            unsupported(r#".a.@bogus = "x""#),
            UnsupportedConstruct::AccessorAssignment
        );
        let href = compiled(r#".a.&href |= "u""#);
        assert!(href.fact_writes(), "an admitted attribute write lowers to FactAssign");
    }

    #[test]
    fn admitted_fact_roles_compile_without_edit_mode() {
        // The write allow-list admits the three comment-position selectors
        // plus the four METADATA roles of the YAML write channel. Query-time
        // compilation lowers each to FactAssign; `--edit` is the splice lane,
        // not a compile gate.
        for program in [
            r#".a.@comment = ["x"]"#,
            r#".a.@comment_inline = ["x"]"#,
            r#".a.@comment_foot = ["x"]"#,
            r#".a.@style = "double""#,
            r#".a.@tag = "!!str""#,
            r#".a.@anchor = "x""#,
            r#".a.@alias = "x""#,
        ] {
            let compiled = compiled(program);
            assert!(compiled.fact_writes(), "{program} must lower to a FactAssign");
        }
    }

    #[test]
    fn specific_spelling_rejections_skip_the_supported_surface_dump() {
        // rejection whose `describe()` names ONE exact spelling (the accessor
        // family) leads with its own sentence and skips the supported-surface
        // dump. A one-token mistake like `.a.@bogus = "x"` must not print an
        // ~8 KB wall of every registered builtin. (Dynamic selectors COMPILE;
        // only the non-admitted static names stay here.)
        let rendered = format!("{}", unsupported_error(r#".a.@bogus = "x""#));
        assert!(
            !rendered.contains("supported surface"),
            "the rejection must not carry the supported-construct dump: {rendered}"
        );
        assert!(
            !rendered.contains("builtins:"),
            "the rejection must not name the builtin list: {rendered}"
        );
    }

    #[test]
    fn node_accessor_lowers_to_an_accessor_step() {
        // The direct `.@name` and quoted `.@["name"]` forms now lower to real
        // `StepAccess::NodeAccessor` steps carrying the decoded selector text.
        let direct = compiled(".@tag");
        assert!(matches!(root_stage_steps(&direct), [StageStep { .. }]));
        assert!(matches!(
            root_stage_steps(&direct)[0].access(),
            StepAccess::NodeAccessor(name) if name == "tag"
        ));

        let quoted = compiled(r#".@["comment"]"#);
        assert!(matches!(
            root_stage_steps(&quoted)[0].access(),
            StepAccess::NodeAccessor(name) if name == "comment"
        ));

        // An escaped quoted selector decodes with the escape set.
        let escaped = compiled(r#".@["leading\nnote"]"#);
        assert!(matches!(
            root_stage_steps(&escaped)[0].access(),
            StepAccess::NodeAccessor(name) if name == "leading\nnote"
        ));
    }

    #[test]
    fn attribute_accessor_lowers_to_an_attribute_step() {
        let direct = compiled(".&href");
        assert!(matches!(
            root_stage_steps(&direct)[0].access(),
            StepAccess::Attribute(name) if name == "href"
        ));

        let quoted = compiled(r#".&["data-id"]"#);
        assert!(matches!(
            root_stage_steps(&quoted)[0].access(),
            StepAccess::Attribute(name) if name == "data-id"
        ));
    }

    #[test]
    fn accessor_steps_compose_with_ordinary_postfix_paths() {
        // `.price.@tag` and `.a.&href` are one fused stage: an ordinary key step
        // followed by the accessor step.
        let node = compiled(".price.@tag");
        assert!(matches!(root_stage_steps(&node), [StageStep { .. }, StageStep { .. }]));
        assert!(matches!(
            root_stage_steps(&node)[0].access(),
            StepAccess::Key(name) if name == "price"
        ));
        assert!(matches!(
            root_stage_steps(&node)[1].access(),
            StepAccess::NodeAccessor(name) if name == "tag"
        ));

        let attribute = compiled(".a.&href");
        assert!(matches!(
            root_stage_steps(&attribute)[0].access(),
            StepAccess::Key(name) if name == "a"
        ));
        assert!(matches!(
            root_stage_steps(&attribute)[1].access(),
            StepAccess::Attribute(name) if name == "href"
        ));
    }

    #[test]
    fn iteration_compiles_to_an_each_step() {
        // `.[]` is now in the subset: a single `Each` step, whole-document
        // pushdown (empty prefix), the entire stage as residual.
        let program = compiled(".[]");
        assert!(program.program.split().is_whole_document());
        assert_eq!(program.program.split().prefix_len(), 0);
        assert_eq!(root_stage_steps(&program).len(), 1);
    }

    #[test]
    fn prefixed_iteration_pushes_the_prefix_and_keeps_the_residual() {
        // `.a[].b`: prefix `.a` pushed down (len 1); residual `[Each, b]`.
        let program = compiled(".a[].b");
        assert!(!program.program.split().is_whole_document());
        assert_eq!(program.program.split().prefix_len(), 1);
        assert_eq!(root_stage_steps(&program).len(), 3);
    }

    #[test]
    fn prefixed_iteration_lowers_the_element_whole_document() {
        // `.a[].b` is an ELEMENT row (the bare fan-out), so it lowers the
        // LAZY WHOLE-DOCUMENT requirement with the element demand hint — the
        // codec's span skeleton must survive for the document-core consumer
        // to iterate it — where `.a` keeps the scoped forward route. The
        // prefix pushdown of the program's SPLIT is unchanged (`.a`,
        // length 1); only the requirement route moved, exactly as the count
        // rows did.
        let prefixed = requirement_of(".a[].b");
        let bare = requirement_of(".a");
        assert_ne!(prefixed.footprint(), bare.footprint());
        assert!(prefixed.footprint().is_whole());
        assert_eq!(prefixed.result(), AccessResultKind::CompleteDocument);
        assert!(prefixed.element().is_some());
        assert!(!bare.footprint().is_whole());
        assert_eq!(bare.result(), AccessResultKind::Located);
    }

    #[test]
    fn a_general_slice_bound_becomes_a_binder_frame() {
        // Both bounds take the index operand's frame, and `start` ends up OUTSIDE
        // `end`: `start` is generated first, so `end`'s forks are younger and it
        // varies faster (`[0,1,2,3,4] | [.[(0,1):(3,4)]]` is
        // `[[0,1,2],[0,1,2,3],[1,2],[1,2,3]]`).
        let program = compiled(".[.a:.b]");
        let nodes = program.program.nodes();
        let ProgramNode::Bind {
            slot: start_slot, body, ..
        } = &nodes[program.program.root().index()]
        else {
            panic!("a general slice bound must lower to a binder frame");
        };
        let ProgramNode::Bind {
            slot: end_slot,
            body: stage,
            ..
        } = &nodes[body.index()]
        else {
            panic!("`start` must sit outside `end`");
        };
        let ProgramNode::Stage { steps, .. } = &nodes[stage.index()] else {
            panic!("the frames must wrap the slice's stage");
        };
        let StepAccess::Slice(bounds) = steps[0].access() else {
            panic!("the step must stay a slice");
        };
        assert!(matches!(bounds.start, SliceBound::Var(slot) if slot == *start_slot));
        assert!(matches!(bounds.end, SliceBound::Var(slot) if slot == *end_slot));
    }

    #[test]
    fn a_constant_slice_bound_still_needs_no_binder() {
        // `.[1:2]`, an open bound and a bare `$var` keep their verbatim spellings,
        // which is what every landed slice-route receipt keys off.
        for source in [".[1:2]", ".[:2]", ".[-1:]", "1 as $x | .[$x:]"] {
            let program = compiled(source);
            let binders = program
                .program
                .nodes()
                .iter()
                .filter(|node| matches!(node, ProgramNode::Bind { .. }))
                .count();
            assert_eq!(
                binders,
                usize::from(source.contains(" as ")),
                "{source} must not grow a binder"
            );
        }
    }

    /// The authored bound pair of the program's ONE slice step (panics
    /// otherwise), as the bounds the pushdown reads them — the very object
    /// `try_normalize` / the recognizers consume.
    fn one_slice(program: &CompiledProgram) -> &crate::program::SliceBounds {
        let mut found: Option<&crate::program::SliceBounds> = None;
        for node in program.program.nodes() {
            let ProgramNode::Stage { steps, .. } = node else {
                continue;
            };
            for step in steps {
                if let StepAccess::Slice(bounds) = step.access() {
                    assert!(found.is_none(), "expected exactly one slice step");
                    found = Some(bounds);
                }
            }
        }
        found.expect("expected exactly one slice step")
    }

    #[test]
    fn a_computed_constant_slice_bound_folds_to_its_literal() {
        // `.[(1+2):]` folds to the authored `.[3:]` at lower time,
        // so the range-projection early stop and every recognizer that keys off
        // an authored literal bound engage without change. The fold is exact —
        // the SAME arithmetic the executor runs, so the literal IS what the
        // runtime frame would have produced.
        let program = compiled(".[(1+2):]");
        let bounds = one_slice(&program);
        assert_eq!(bounds.try_normalize(), Some((Some(3), None)));
        let program = compiled(".[(10-2):(1+2)]");
        let bounds = one_slice(&program);
        assert_eq!(bounds.try_normalize(), Some((Some(8), Some(3))));
        // Nested groups fold through the same set: `((1+2)+3)` is 6.
        let program = compiled(".[((1+2)+3):(2+2)]");
        let bounds = one_slice(&program);
        assert_eq!(bounds.try_normalize(), Some((Some(6), Some(4))));
        // `(0-1)` folds to the authored -1 — a folded negative bound normalizes
        // exactly as an authored `.[-1:]` does: the signed value rides the
        // range and the codec's two-pass arm resolves it against the observed
        // container length.
        let program = compiled(".[(0-1):]");
        let bounds = one_slice(&program);
        assert!(matches!(bounds.start, SliceBound::Literal(_)));
        assert_eq!(bounds.try_normalize(), Some((Some(-1), None)));
    }

    #[test]
    fn a_constant_bound_variable_folds_to_its_binding() {
        // `1 as $x | .[$x:]` folds the slice bound to the literal the binder
        // provably delivers — byte-for-byte the authored `.[1:]`. The binder's
        // own `Bind` frame still runs; only the slice's slot read is replaced.
        let program = compiled("1 as $x | .[$x:]");
        let bounds = one_slice(&program);
        assert_eq!(bounds.try_normalize(), Some((Some(1), None)));
        // Both bounds, and the innermost binder wins on a shadowing re-bind.
        let program = compiled("2 as $x | 1 as $y | .[$y:$x]");
        let bounds = one_slice(&program);
        assert_eq!(bounds.try_normalize(), Some((Some(1), Some(2))));
        let program = compiled("1 as $x | 2 as $x | .[$x:]");
        let bounds = one_slice(&program);
        assert_eq!(bounds.try_normalize(), Some((Some(2), None)));
        // A non-number constant binding folds too: `null` behaves as Open at
        // runtime, exactly as the Var path would have resolved it.
        let program = compiled("null as $x | .[$x:]");
        let bounds = one_slice(&program);
        assert!(matches!(bounds.start, SliceBound::Literal(Value::Null)));
        assert_eq!(bounds.try_normalize(), Some((None, None)));
        // The fold does NOT require the bound to be usable in the pushdown: a
        // string binding folds to its authored literal and declines exactly as
        // an authored non-numeric bound does (the runtime raises the
        // integer-bound class over an array either way).
        let program = compiled("\"abc\" as $x | .[$x:2]");
        let bounds = one_slice(&program);
        assert!(matches!(bounds.start, SliceBound::Literal(Value::String(_))));
        assert_eq!(bounds.try_normalize(), None);
    }

    #[test]
    fn a_dynamic_bound_variable_stays_a_var_slot() {
        // The decline discipline: anything the fold cannot prove stays dynamic,
        // exactly as before. A bind whose source is not a lower-time constant
        // (an input read, a multi-output generator, a destructuring projection,
        // a function argument) leaves the slice bound a `Var` slot.
        for source in [
            ".[0] as $x | .[$x:]",
            "(1,2) as $x | .[$x:]",
            "{a:1} as {$x} | .[$x:]",
            "def f($x): .[$x:]; f(3)",
        ] {
            let program = compiled(source);
            let bounds = one_slice(&program);
            assert!(
                matches!(bounds.start, SliceBound::Var(_)),
                "{source} must keep a Var bound"
            );
            assert_eq!(bounds.try_normalize(), None, "{source} must decline");
        }
        // A destructuring projection DOES bind a slot (the frame exists); only
        // the fold declines — the runtime value is still the projected member.
        let program = compiled("{a:1} as {$x} | .[$x:]");
        assert!(
            program
                .program
                .nodes()
                .iter()
                .any(|node| matches!(node, ProgramNode::Bind { .. }))
        );
    }

    #[test]
    fn a_non_constant_computed_bound_still_takes_a_binder_frame() {
        // The `+`/`-` fold declines every operand that is not a provably-
        // constant NUMBER: an input read, a non-additive operator, a non-number
        // operand (string concat, the null additive identity — the latter fires
        // a mismatch cell at runtime, which a fold must never skip), and a bound
        // variable inside the expression.
        for source in [
            ".[(.a+1):]",
            ".[(1*2):]",
            ".[(1/2):]",
            ".[(1-\"a\"):]",
            ".[(\"a\"+\"b\"):]",
            ".[(null+1):]",
            "2 as $x | .[($x+1):]",
        ] {
            let program = compiled(source);
            let bounds = one_slice(&program);
            assert!(
                matches!(bounds.start, SliceBound::Var(_)),
                "{source} must keep a Var bound"
            );
            assert_eq!(bounds.try_normalize(), None, "{source} must decline");
            assert!(
                program
                    .program
                    .nodes()
                    .iter()
                    .any(|node| matches!(node, ProgramNode::Bind { .. })),
                "{source} must keep its operand frame"
            );
        }
    }

    #[test]
    fn a_folded_computed_bound_matches_its_authored_literal_requirement() {
        // The range-projection law, at the route-eligibility level: a folded
        // computed bound is INDISTINGUISHABLE from the authored literal — same
        // eligibility, same lowered exact-path footprint, same result
        // authority. This is the byte-identity the corpus pins at run time.
        let resources = resources();
        for (folded, authored) in [(".[(1+2):]", ".[3:]"), (".catalog[(10-2):(1+2)]", ".catalog[8:3]")] {
            assert!(compiled(folded).range_locate_eligible(), "{folded} must route");
            let folded_req = compiled(folded)
                .try_range_locate_requirement(&resources)
                .expect("the folded range-locate requirement lowers");
            let authored_req = compiled(authored)
                .try_range_locate_requirement(&resources)
                .expect("the authored range-locate requirement lowers");
            assert_eq!(
                folded_req.footprint().fingerprint(),
                authored_req.footprint().fingerprint(),
                "{folded} must lower the same footprint as {authored}"
            );
            assert_eq!(folded_req.result(), authored_req.result());
        }
        // The decline twin: a non-foldable computed bound keeps the floor — it
        // is not silently given the literal's fast lane.
        assert!(!compiled(".[(1*2):]").range_locate_eligible());
    }

    #[test]
    fn per_component_optional_suffix_is_accepted_and_flagged() {
        // `.a?` compiles: one key step carrying the per-component optional flag.
        assert_eq!(keys(&compiled(".a?")), ["a"]);
        assert_eq!(optional_flags(&compiled(".a?")), [true]);
    }

    #[test]
    fn optional_flag_is_per_component_within_a_chain() {
        // `.a?.b` flags only the first component; `.a.b?` flags only the second.
        assert_eq!(keys(&compiled(".a?.b")), ["a", "b"]);
        assert_eq!(optional_flags(&compiled(".a?.b")), [true, false]);
        assert_eq!(optional_flags(&compiled(".a.b?")), [false, true]);
    }

    #[test]
    fn optional_suffix_on_indices_and_bracket_keys() {
        assert_eq!(indices(&compiled(".[0]?")), [0]);
        assert_eq!(optional_flags(&compiled(".[0]?")), [true]);
        assert_eq!(keys(&compiled(r#".["a"]?"#)), ["a"]);
        assert_eq!(optional_flags(&compiled(r#".["a"]?"#)), [true]);
    }

    #[test]
    fn standalone_term_suppression_lowers_to_a_catchless_try() {
        // FENCE FLIP: `.?` is the whole-term `try` sugar — now legal, a
        // catchless `Try` over identity (`5 | .?` → 5).
        let program = compiled(".?");
        assert!(matches!(
            program.program.nodes()[program.program.root().index()],
            ProgramNode::Try { handler: None, .. }
        ));
    }

    #[test]
    fn second_optional_lowers_as_term_suppression_try() {
        // FENCE FLIP: the first `?` of `.a??` is the per-component flag; the second
        // is a whole-term `try` — now a catchless `Try` wrapping the `.a?` prefix.
        let program = compiled(".a??");
        assert!(matches!(
            program.program.nodes()[program.program.root().index()],
            ProgramNode::Try { handler: None, .. }
        ));
    }

    #[test]
    fn a_general_index_operand_becomes_a_binder_frame() {
        // `.[e]` is `e as $t | .[$t]`: the operand is hoisted into a `Bind` whose
        // body indexes through the slot it just filled. An interpolated field key
        // (`."\(1)"`) is the same form under different punctuation, and a
        // non-integer numeric index is just an operand the runtime truncates.
        for source in [".[.a]", ".[1+1]", ".[1.5]", ".[-1.5]", r#"."\(1)""#, r#".["a"+"b"]"#] {
            let program = compiled(source);
            let nodes = program.program.nodes();
            let ProgramNode::Bind { slot, body, .. } = &nodes[program.program.root().index()] else {
                panic!("{source} must lower to a binder frame");
            };
            let ProgramNode::Stage { steps, .. } = &nodes[body.index()] else {
                panic!("{source} must bind a stage");
            };
            assert!(
                matches!(steps[0].access(), StepAccess::DynVar(bound) if bound == slot),
                "{source} must read the slot it bound"
            );
        }
    }

    #[test]
    fn a_constant_index_operand_still_needs_no_binder() {
        // The fast paths that keep `.a[0]`, `.["k"]` and `.[$x]` at exactly their
        // pre-vertical cost: one fused step, and no binder frame beyond the ones
        // the user authored.
        for source in [".[0]", ".[-1]", r#".["k"]"#, ".a[0]", "1 as $x | .[$x]"] {
            let program = compiled(source);
            let binders = program
                .program
                .nodes()
                .iter()
                .filter(|node| matches!(node, ProgramNode::Bind { .. }))
                .count();
            let authored = usize::from(source.contains(" as "));
            assert_eq!(binders, authored, "{source} must not grow a binder");
        }
    }

    #[test]
    fn recursive_descent_lowers_to_one_descend_step() {
        // `..` is in scope now: it lowers to a current-start stage carrying one
        // `Descend` step, so a postfix chain fuses onto it like any other path.
        let program = compiled("..");
        let steps = root_stage_steps(&program);
        assert_eq!(steps.len(), 1);
        assert!(matches!(steps[0].access(), StepAccess::Descend));
    }

    #[test]
    fn pipe_of_paths_fuses_to_one_stage() {
        // `.a | .b` fuses to the same single stage as `.a.b`.
        let program = compiled(".a | .b");
        assert!(!program.program.split().is_whole_document());
        assert_eq!(program.program.split().prefix_len(), 2);
        assert_eq!(keys(&program), ["a", "b"]);
        assert_eq!(optional_flags(&program), [false, false]);
    }

    #[test]
    fn pipe_chain_of_three_fuses_in_order() {
        let program = compiled(".a | .b | .c");
        assert_eq!(keys(&program), ["a", "b", "c"]);
    }

    #[test]
    fn optional_flags_travel_with_fusion_across_a_pipe() {
        // `.a? | .b` flags the first fused step; `.a | .b?` the second.
        assert_eq!(optional_flags(&compiled(".a? | .b")), [true, false]);
        assert_eq!(optional_flags(&compiled(".a | .b?")), [false, true]);
    }

    #[test]
    fn identity_pipe_identity_is_whole_document() {
        // Fusing two empty stages yields the empty stage: whole-document identity.
        let program = compiled(". | .");
        assert!(program.program.split().is_whole_document());
        assert!(root_stage_steps(&program).is_empty());
    }

    #[test]
    fn pipe_of_paths_requirement_is_identical_to_the_fused_path() {
        // The fusion perf thesis: `.a | .b` produces a structurally identical
        // AccessRequirement to `.a.b` (account-independent structural equality).
        let piped = requirement_of(".a | .b");
        let fused = requirement_of(".a.b");
        assert_eq!(piped.footprint(), fused.footprint());
        assert_eq!(piped.result(), fused.result());
        assert_eq!(piped.footprint().fingerprint(), fused.footprint().fingerprint());
    }

    #[test]
    fn optional_flags_do_not_alter_the_requirement() {
        // Optionality is interpretation, never access: `.a?.b`, `.a.b?`,
        // `.a? | .b`, and `.a.b` all lower to the identical requirement.
        let plain = requirement_of(".a.b");
        for source in [".a?.b", ".a.b?", ".a? | .b", ".a | .b?"] {
            let marked = requirement_of(source);
            assert_eq!(
                marked.footprint(),
                plain.footprint(),
                "{source} must not alter the requirement footprint"
            );
            assert_eq!(marked.result(), plain.result());
        }
    }

    #[test]
    fn comma_lowers_to_a_choice_root() {
        // `.a, .b` is now in the subset: a top-level `Choice`, whole-document
        // pushdown (both members share one document authority).
        let program = compiled(".a, .b");
        assert!(program.program.split().is_whole_document());
        assert!(matches!(
            program.program.nodes()[program.program.root().index()],
            ProgramNode::Choice { .. }
        ));
    }

    #[test]
    fn three_member_comma_is_left_associative() {
        // `.a, .b, .c` nests as `Choice(Choice(a, b), c)`: the root's left is a
        // Choice and its right is a Stage.
        let program = compiled(".a, .b, .c");
        let nodes = program.program.nodes();
        let ProgramNode::Choice { left, right } = &nodes[program.program.root().index()] else {
            panic!("comma root must be a Choice");
        };
        assert!(matches!(nodes[left.index()], ProgramNode::Choice { .. }));
        assert!(matches!(nodes[right.index()], ProgramNode::Stage { .. }));
    }

    #[test]
    fn transparent_group_over_a_path_is_the_bare_stage() {
        // `(.a).b` and `((.a))` are transparent: groups produce no node, so the
        // postfix fuses onto the base stage exactly as `.a.b` / `.a` would.
        assert_eq!(keys(&compiled("(.a).b")), ["a", "b"]);
        assert_eq!(keys(&compiled("((.a))")), ["a"]);
        assert_eq!(keys(&compiled("(.a | .b).c")), ["a", "b", "c"]);
    }

    #[test]
    fn group_postfix_on_a_choice_is_a_flatmap() {
        // `(.a, .b).c` composes the postfix `.c` onto a `Choice` base, which is
        // not a Stage, so it becomes `FlatMap(Choice, Stage[c])` — identical to
        // `(.a, .b) | .c`.
        let program = compiled("(.a, .b).c");
        let nodes = program.program.nodes();
        let ProgramNode::FlatMap { upstream, body } = &nodes[program.program.root().index()] else {
            panic!("group-postfix on a choice must be a FlatMap");
        };
        assert!(matches!(nodes[upstream.index()], ProgramNode::Choice { .. }));
        assert!(matches!(nodes[body.index()], ProgramNode::Stage { .. }));
        assert!(program.program.split().is_whole_document());
    }

    #[test]
    fn pipe_into_a_group_choice_keeps_the_flatmap_and_pushes_the_prefix() {
        // `.a | (.b, .c)` keeps `FlatMap(Stage[a], Choice)`; the static prefix
        // `.a` sits in the FlatMap upstream and is still pushed down.
        let program = compiled(".a | (.b, .c)");
        let nodes = program.program.nodes();
        assert!(matches!(
            nodes[program.program.root().index()],
            ProgramNode::FlatMap { .. }
        ));
        assert!(!program.program.split().is_whole_document());
        assert_eq!(program.program.split().prefix_len(), 1);
    }

    #[test]
    fn a_total_static_collect_body_hoists_its_whole_path() {
        // `[.a.b]` rewrites to `.a.b | [.]` — the same hoist the constructor gets
        // — so the path a bare collect could never contribute reaches the codec
        // as a pushdown prefix instead of costing the whole document to read one
        // element.
        for (source, prefix) in [("[.a]", 1), ("[.a.b]", 2), ("[.a[0].b]", 3), ("[.a | .b]", 2)] {
            let program = compiled(source);
            assert!(!program.program.split().is_whole_document(), "{source}");
            assert_eq!(program.program.split().prefix_len(), prefix, "{source}");
        }
        // The rewrite is all-or-nothing, so the residual is exactly `[.]`.
        let program = compiled("[.a.b]");
        let nodes = program.program.nodes();
        let ProgramNode::FlatMap { body, .. } = &nodes[program.program.root().index()] else {
            panic!("the hoist must leave a FlatMap root");
        };
        let ProgramNode::CollectArray { body: Some(body) } = &nodes[body.index()] else {
            panic!("the hoisted body must still be the collect");
        };
        let ProgramNode::Stage { steps, .. } = &nodes[body.index()] else {
            panic!("the collect residual must be a stage");
        };
        assert!(steps.is_empty(), "the residual is exactly `[.]`");
        // The declining default, each decline for its own law.
        for (source, law) in [
            ("[]", "no body to hoist"),
            ("[1]", "a literal-start body walks no path"),
            (
                "[.a?]",
                "an optional step may emit ZERO, and `[empty]` is `[]` while \
                 `empty | [.]` publishes nothing at all",
            ),
            ("[.a, .b]", "a comma body has two producers and no single path"),
        ] {
            assert!(
                compiled(source).program.split().is_whole_document(),
                "{source} must decline the collect hoist ({law})"
            );
        }
    }

    #[test]
    fn a_collect_barrier_is_transparent_to_the_pushdown_spine() {
        // `[f]` evaluates `f` against the collect's own input, so the barrier has
        // ONE producer whose dot IS the collect's — and the spine descends it
        // like a `FlatMap` upstream. This is the shape the all-or-nothing hoist
        // above declines (its residual would keep a suffix), and where the whole
        // 10k-element document used to be read to answer a one-key demand.
        for (source, prefix) in [
            ("[.a[]]", 1),
            ("[.a[] | .b]", 1),
            ("[.users[].id]", 1),
            ("[.a | length]", 1),
            ("[.a.b[].c] | sort", 2),
        ] {
            let program = compiled(source);
            assert!(!program.program.split().is_whole_document(), "{source}");
            assert_eq!(program.program.split().prefix_len(), prefix, "{source}");
        }
        // BENEATH the barrier the prefix stops at the first OPTIONAL step: a
        // pushed-down suppression publishes nothing, while `[.a.b?]` over
        // `{"a": 5}` is `[]`. `.a` still pushes down; the flagged step stays in
        // the residual, where the collect can see it emit nothing.
        assert_eq!(compiled("[.a.b?]").program.split().prefix_len(), 1);
        assert!(compiled("[.a?]").program.split().is_whole_document());
    }

    #[test]
    fn a_spine_node_hoists_the_prefix_its_operands_share() {
        // The four multi-producer spine families read one dot through several
        // graphs, so what they can contribute is the prefix-lattice JOIN of
        // those graphs — the codec carries one exact path per requirement, so
        // their UNION is still not expressible and their join is.
        for (source, prefix) in [
            // Conditional: condition + the arm that runs, `null` exempt.
            ("if .a.b then .a.c else null end", 1),
            ("if .u[0].active then .u[0].email else null end", 2),
            ("if .a.b then .a.c else .a.d end", 1),
            // Alternative and the logical pair.
            (".a.b // .a.c", 1),
            (".a.b and .a.c", 1),
            (".a.b or .a.c", 1),
            // Binary, including operands whose leading stage sits under a pipe:
            // `.users | map(.age) | add` contributes `.users`.
            (".a.b + .a.c", 1),
            ("1 + .a.b", 2),
            // The join stops AT the first optional step rather than declining:
            // `.p` is total, and the `?` stays in the residual where a suppressed
            // step is still the alternative's own zero-output left operand.
            (".p.a? // 1", 1),
            ("(.users | map(.age) | add) / (.users | length)", 1),
            // The hoist COMPOSES: an inner spine node that hoisted its own join
            // became a `FlatMap` whose leading stage the outer join then reads,
            // so an `elif` ladder contributes the prefix its whole chain shares.
            ("if .a.b then .a.c elif .a.d then .a.e else .a.f end", 1),
            // 014 Part A: an `==` operand joins like its `!=`/`<` siblings. The
            // probe-equality guard (`join::is_probe_equality`) protects only a
            // SELECT predicate's leftmost conjunct — the one context a scan row
            // can read — so a conditional condition, a logical/alternative/
            // binary operand, or a top-level comparison may hoist its shared
            // prefix freely.
            ("if .a.b == 1 then .a.c else .a.d end", 1),
            ("if (.a.b == 1 and .a.c > 0) then .a.d else .a.e end", 1),
            ("(.a.b == 1) // .a.c", 1),
            ("(.a.b == 1) and .a.c", 1),
            ("(.a.b == 1) or .a.c", 1),
            ("(.a.b == 1) + .a.c", 1),
            (".a.b == 1", 2),
        ] {
            let program = compiled(source);
            assert!(!program.program.split().is_whole_document(), "{source}");
            assert_eq!(program.program.split().prefix_len(), prefix, "{source}");
        }
    }

    #[test]
    fn a_spine_hoist_declines_every_shape_it_cannot_prove() {
        for (source, law) in [
            // Operands that diverge at their first step have an empty join. The
            // demand-union plan wanted `{.a.b, .c, .d}` here; the codec's
            // footprint vocabulary has no union form, so this keeps the floor.
            ("if .a.b then .c else .d end", "an empty join is no prefix"),
            (
                "if (.a.b == 1) then .c else .d end",
                "an `==` operand joins no more than its bare sibling — the arms \
                 still diverge at the first step",
            ),
            (".a // .b", "the same, for the alternative"),
            // The witness rule: only the condition is evaluated unconditionally,
            // so a literal condition cannot license a prefix the arms share —
            // the answer is `1` for this program over a scalar input.
            ("if true then 1 else .p.x end", "no always-evaluated witness"),
            (
                ".p?.a // 1",
                "a LEADING optional step may emit ZERO, so the join is empty",
            ),
            // Poisoned operands: anything that is neither an exempt literal nor
            // a graph with a `Current`-start leading stage.
            (
                "if length then .a.c else .a.d end",
                "a call has no leading stage, so the witness cannot walk a prefix",
            ),
            (
                "if .a.b then length else .a.d end",
                "a call ARM poisons the node just as a call condition does",
            ),
            (
                "if .a.b then (.a.c, .a.d) else .a.e end",
                "a comma operand is unresolvable",
            ),
            (
                "if .a.b then (try .a.c) else .a.d end",
                "a `try` operand keeps its whole-input authority",
            ),
            (
                ".k as $x | (.[$x].a + .[$x].b)",
                "a LEADING dynamic step is not one the codec resolves",
            ),
        ] {
            assert!(
                compiled(source).program.split().is_whole_document(),
                "{source} must decline the spine hoist ({law})"
            );
        }
    }

    #[test]
    fn a_spine_hoist_truncates_a_join_longer_than_the_bound() {
        // The bound is `MAX_HOISTED_PREFIX` (8). These operands share NINE
        // steps; the ninth stays in the residual, where it was before the
        // hoist existed.
        let program = compiled(".a.b.c.d.e.f.g.h.i.j + .a.b.c.d.e.f.g.h.i.k");
        assert_eq!(program.program.split().prefix_len(), 8);
    }

    #[test]
    fn group_choice_equivalence_shares_the_requirement() {
        // `(.a, .b).c` ≡ `(.a, .b) | .c`: identical requirement (both whole
        // document, both graph-rooted at the same FlatMap(Choice, Stage[c])).
        let postfix = requirement_of("(.a, .b).c");
        let piped = requirement_of("(.a, .b) | .c");
        assert_eq!(postfix.footprint(), piped.footprint());
        assert_eq!(postfix.result(), piped.result());
    }

    #[test]
    fn constructor_and_literal_bases_are_accepted() {
        // Postfix on a constructor or literal base now compiles (`[1,2][0]` → 1,
        // `{a: .b}.a`, `3[]?`): the array/object/literal bases join identity and
        // group as accepted postfix bases.
        for source in ["[1,2][0]", "{a: .b}.a", "3[]?", "(1).a?", "[.[]][0]"] {
            assert!(compile(source).is_ok(), "{source} must compile");
        }
    }

    #[test]
    fn every_expression_is_a_postfix_base() {
        // FENCE RETIRED: the base position carries no law of its own — `BASE
        // STEPS` is `BASE | .STEPS` — so the last shapes the bootstrap called a
        // "non-identity base" now compose like every other base. `empty[0]`
        // yields nothing, `(reduce …)[0]` indexes the fold's result, and
        // `if … end []` iterates the taken branch (`if true then [.] else .
        // end []` over `null` is `null`).
        for source in [
            "empty[0]",
            "reduce .[] as $x (0; .)[0]",
            "foreach .[] as $x (0; .)[0]",
            "if true then [.] else . end []",
            "if true then [.] else . end[0]?",
        ] {
            assert!(compile(source).is_ok(), "{source} must compile");
        }
    }

    #[test]
    fn a_base_that_did_not_parse_is_still_refused() {
        // …and the widening must not swallow a recovered parse. It cannot: the
        // parse rejection is raised BEFORE lowering runs, so a base whose
        // expression is an `ExprKind::Error` never reaches `lower_postfix_base`
        // at all. Pinned here because the widened base is exactly the place
        // where a silently-composed error node would be invisible.
        assert!(
            matches!(compile("(|)[0]"), Err(EngineCompileError::Parse(_))),
            "a malformed base must be a parse rejection"
        );
    }

    #[test]
    fn a_format_filter_is_a_postfix_base() {
        // FENCE FLIP: `@text[0:1]` parses as a postfix chain on the format
        // filter, which is why `{"a":1} | @text.a` reports the INDEX refusal at
        // RUN time rather than failing to parse. The chain therefore has to
        // lower, and what it lowers to is a `format` call with the steps
        // composed after it.
        for source in ["@base64[0]", "@text[0:1]", "@text.a", "@base64 \"x\\(.)\"[0]"] {
            let program = compiled(source);
            assert!(
                matches!(
                    program.program.nodes()[program.program.root().index()],
                    ProgramNode::FlatMap { .. }
                ),
                "a format base with a postfix step lowers to a mapped chain: {source}"
            );
        }
    }

    #[test]
    fn format_template_object_shorthand_looks_up_the_produced_key() {
        // `{@text "k"}` is object shorthand: the format produces the key, the
        // value is a lookup of that key on the input. On null input the
        // lookup is null, so the constructor emits `{"k":null}`.
        use crate::exec::{EngineRun, RunPoll};
        use jqf_builtins::codec_result::{CodecInputOutcome, EngineResult};
        let mut resources = resources();
        let compiled = compiled(r#"{@text "k"}"#);
        let mut stream = match compiled
            .try_run_whole_value(CodecInputOutcome::Result(EngineResult::owned(Value::Null)), &resources)
            .expect("run")
        {
            EngineRun::Stream { stream, .. } => stream,
            other => panic!("expected a stream: {other:?}"),
        };
        let item = match stream.poll(&mut resources) {
            Ok(RunPoll::Item(EngineResult::Owned(value))) => {
                jqf_builtins::semantics::render::to_json(&value).expect("json")
            }
            other => panic!("expected one owned object: {other:?}"),
        };
        assert_eq!(item, r#"{"k":null}"#);
        assert!(matches!(stream.poll(&mut resources), Ok(RunPoll::Complete)));
    }

    #[test]
    fn group_term_suppression_lowers_to_a_catchless_try() {
        // FENCE FLIP: `(.a)?` and `(.a, .b)?` are a `?` on a whole GROUP term —
        // the whole-term `try` — now legal, a catchless `Try` over the group
        // body.
        for source in ["(.a)?", "(.a, .b)?"] {
            let program = compiled(source);
            assert!(
                matches!(
                    program.program.nodes()[program.program.root().index()],
                    ProgramNode::Try { handler: None, .. }
                ),
                "{source} lowers to a catchless Try"
            );
        }
    }

    #[test]
    fn builtin_calls_compile() {
        // The seed builtins now resolve and compile (bare and parenthesized).
        for source in [
            "length",
            "keys",
            "select(.a)",
            "map(.id)",
            "map(.id)[0]",
            ".items | map(.id) | length",
            "not",
            "true | not",
        ] {
            assert!(compile(source).is_ok(), "{source} must compile");
        }
    }

    #[test]
    fn control_flow_constructs_compile() {
        // The control-flow vertical: `if`/`elif`/`else` (and the else-less form),
        // `and`/`or`, and `//`. All were rejected before this vertical; each now
        // compiles to a graph-rooted program.
        for source in [
            "if .a then 1 else 2 end",
            "if .a then 1 end",
            "if .a then 1 elif .b then 2 else 3 end",
            ".a and .b",
            ".a or .b",
            ".a // .b",
            ".a // .b // .c",
            ".x | if . then 1 else 2 end",
        ] {
            let program = compile(source).expect("compiles");
            assert!(
                !matches!(
                    program.program.nodes()[program.program.root().index()],
                    ProgramNode::Stage { .. }
                ),
                "{source} lowers to a control-flow graph (never a bare Stage)"
            );
        }
    }

    #[test]
    fn unknown_and_wrong_arity_calls_are_undefined() {
        for (source, name, arity) in [
            ("nonexistent", "nonexistent", 0),
            ("length(1)", "length", 1),
            ("map", "map", 0),
            ("select", "select", 0),
        ] {
            match compile(source) {
                Err(EngineCompileError::UndefinedCall {
                    name: got_name,
                    arity: got_arity,
                    ..
                }) => {
                    assert_eq!(got_name, name, "{source}");
                    assert_eq!(got_arity, arity, "{source}");
                }
                other => panic!("expected {source:?} undefined, got {other:?}"),
            }
        }
    }

    #[test]
    fn map_lowers_to_the_same_plan_as_the_bracket_pipe_expansion() {
        // `map(f)` ≡ `[.[] | f]`: identical requirement (both whole-document,
        // both graph-rooted at the same CollectArray). The Lowering IS the plan.
        let mapped = requirement_of("map(.id)");
        let expanded = requirement_of("[.[] | .id]");
        assert_eq!(mapped.footprint(), expanded.footprint());
        assert_eq!(mapped.result(), expanded.result());
    }

    #[test]
    fn binding_and_loop_constructs_compile() {
        // The reduce/foreach + `as`-bindings vertical: bindings, both loop
        // arities, `empty`, bound-variable postfix navigation, and `.[$x]`.
        for source in [
            ". as $x | $x",
            ".[] as $x | [$x, $x * 2]",
            ". as $x | 2 as $x | $x",
            "reduce .[] as $x (0; . + $x)",
            "reduce .[] as $x (.k; . + $x)",
            "foreach .[] as $x (0; . + $x)",
            "foreach .[] as $x (0; . + $x; [$x, .])",
            "empty",
            "empty as $x | 1",
            "1 + reduce .[] as $x (0; . + $x)",
            "map(reduce .[] as $x (0; . + $x))",
            ".[] as $row | reduce $row[] as $y (0; . + $y)",
            ".a[0] as $r | $r.b[1]",
            "5 as $v | $v.a?",
            ".k as $x | .o[$x]",
            "1 as $i | .[$i]",
            "1 as $i | .[$i]?",
            "{a: (.b as $v | $v + 1)}",
        ] {
            assert!(compile(source).is_ok(), "{source} must compile");
        }
    }

    #[test]
    fn a_bound_variable_lowers_to_a_variable_start_stage() {
        // `$x` is the third stage START (beside `Current` and `Literal`): the
        // executor reads the slot, never a name.
        let program = compiled(". as $x | $x");
        let nodes = program.program.nodes();
        let ProgramNode::Bind { body, slot, .. } = &nodes[program.program.root().index()] else {
            panic!("a binding lowers to a Bind node");
        };
        assert_eq!(*slot, 0, "the first binder occurrence owns slot 0");
        assert!(
            matches!(
                &nodes[body.index()],
                ProgramNode::Stage {
                    start: StageStart::Variable(0),
                    ..
                }
            ),
            "the body `$x` is a Variable-start stage"
        );
    }

    #[test]
    fn each_binder_occurrence_owns_its_own_slot() {
        // Slots are per OCCURRENCE, never reused across sibling scopes —
        // `1 as $x | ...` and the nested `(10,20) as $x | ...` take
        // distinct slots even though both bind the name `$x`.
        let program = compiled("1 as $x | (((10,20) as $x | $x) | . + $x)");
        assert_eq!(program.program.slots(), 2, "two binder occurrences declare two slots");
    }

    #[test]
    fn a_binder_free_program_declares_no_slots() {
        for source in [".", ".a[].b", ".a, .b", "map(.id)", "if .a then 1 else 2 end"] {
            assert_eq!(
                compiled(source).program.slots(),
                0,
                "{source} binds nothing, so it declares no slots"
            );
        }
    }

    #[test]
    fn a_static_prefix_upstream_of_a_loop_still_pushes_down() {
        // The maximal-prefix pushdown law is unchanged by the binder families:
        // `.catalog | reduce .[] as $x (0; . + 1)` keeps the scoped route on its
        // prefix, and the requirement equals bare `.catalog`'s.
        for source in [
            ".catalog | reduce .[] as $x (0; . + 1)",
            ".catalog | foreach .[] as $x (0; . + 1)",
            ".catalog | (.[] as $x | $x)",
        ] {
            let program = compiled(source);
            assert!(!program.program.split().is_whole_document(), "{source}");
            assert_eq!(program.program.split().prefix_len(), 1, "{source}");
            assert_eq!(
                requirement_of(source).footprint(),
                requirement_of(".catalog").footprint(),
                "{source} must push exactly its static prefix down"
            );
        }
    }

    #[test]
    fn a_destructuring_pattern_lowers_to_an_extraction_frame() {
        // Every pattern form compiles now, `?//` chains included. The one shape
        // still rejected is a chain in a THREE-argument `foreach`, and it keeps
        // its own named message rather than a generic parse failure.
        for source in [
            ".[] as [$a, $b] | $a",
            ". as {a: $v} | $v",
            ". as [[$a],{b:$c},$d] | [$a,$c,$d]",
            r#". as {$x, "y": $y, ("z"): $z, k: [$w]} | [$x,$y,$z,$w]"#,
            "reduce .[] as [$a,$b] (0; .+$a+$b)",
            "foreach .[] as {a:$v} (0; .+$v; [$v,.])",
            ".[] as [$a] ?// $a | $a",
            ".[] as {$a, b:[$c]} ?// [$a, {$b}] ?// $d | [$a,$b,$c,$d]",
            "reduce .[] as [$a] ?// $a (0; .+$a)",
            "foreach .[] as [$a] ?// $a (0; .+$a)",
        ] {
            assert!(compile(source).is_ok(), "{source} must compile");
        }
        assert_eq!(
            unsupported("foreach .[] as [$a] ?// $a (0; .+$a; .)"),
            UnsupportedConstruct::Expression("a `?//` destructuring alternative in a three-argument `foreach`")
        );
    }

    #[test]
    fn a_nested_alternative_chain_is_linear_not_exponential() {
        // An arm USED to copy the body, so a chain in the body of a chain
        // squared, and `push_node` refused nests past 13-14. The body is now
        // lowered ONCE behind a `ChainBody` barrier, so a 20-deep nest compiles
        // with a LINEAR arena — the refusal is gone, not moved.
        let nest = |depth: usize| {
            let mut program = String::from(".");
            for _ in 0..depth {
                program.insert_str(0, ". as [$a] ?// $a | (");
                program.push(')');
            }
            program
        };
        // Depth 13 is the old refusal boundary's admission edge: the old
        // per-arm body copies put that nest somewhere between 100k and 200k
        // arena nodes (the bound refused 14). The shared body keeps the same
        // nest tiny, which is the linearity assertion. Deeper nests are exercised
        // through the CLI's big-stack request thread (jqf-cli/tests/
        // deep_program.rs), which is where the engine's no_std test binary
        // cannot follow.
        let outcome = compile(&nest(13)).expect("a nested chain must compile");
        assert!(
            outcome.program.nodes().len() < 20_000,
            "the arena must stay linear, not square: {} nodes",
            outcome.program.nodes().len()
        );
    }

    #[test]
    fn an_alternative_chain_is_one_try_per_loser() {
        // `p1 ?// p2 ?// p3` is a Try chain over one shared matched value, so a
        // three-alternative chain has exactly two handlers — the last
        // alternative is the escape and gets none.
        let program = compiled(". as [$a] ?// {$a} ?// $a | $a");
        let handlers = program
            .program
            .nodes()
            .iter()
            .filter(|node| matches!(node, ProgramNode::Try { handler: Some(_), .. }))
            .count();
        assert_eq!(handlers, 2, "one handler per non-final alternative");
    }

    #[test]
    fn every_alternative_binds_the_chains_whole_name_union() {
        // A name only one alternative mentions is still bound — to `null` — in
        // every other arm, which is why `1 | [. as [$a] ?// $b | [$a,$b]]` is
        // `[[null,1]]` and not an undefined-variable compile error.
        for source in [
            ". as [$a] ?// $b | [$a,$b]",
            ". as [$a,$c] ?// {$b} | [$a,$b,$c]",
            ". as {$a} ?// [$b] ?// $c | [$a,$b,$c]",
        ] {
            assert!(compile(source).is_ok(), "{source} must bind the union");
        }
    }

    #[test]
    fn a_pattern_key_expression_cannot_see_the_patterns_own_variables() {
        // The scope discipline, and the reason the frame is built before any name
        // opens: `. as {k:$k, ($k):$v}` is a compile-time `$k is not defined`.
        assert!(matches!(
            compile(". as {k:$k, ($k):$v} | [$k,$v]"),
            Err(EngineCompileError::UndefinedVariable { .. })
        ));
    }

    #[test]
    fn a_repeated_pattern_name_is_two_slots() {
        // `[1,2] | . as [$a,$a] | $a` is `2`: one slot per binder OCCURRENCE, with
        // the later one shadowing in the body.
        let program = compiled(". as [$a,$a] | $a");
        let nodes = program.program.nodes();
        let slots: Vec<_> = nodes
            .iter()
            .filter_map(|node| match node {
                ProgramNode::Bind { slot, .. } => Some(*slot),
                _ => None,
            })
            .collect();
        let mut unique = slots.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(slots.len(), unique.len(), "no binder may share a slot");
    }

    #[test]
    fn env_lowers_and_loc_is_a_location_literal() {
        // `$ENV` lowers to the `env/0` read, so `$ENV == env` holds by
        // construction; `$__loc__` is the source-location binding and lowers
        // to the literal `{"file": "<top-level>", "line": N}` — the label is
        // the compile source's own, and the line counts from the program text
        // (the corpus pins both spellings).
        assert!(compile("$ENV").is_ok(), "$ENV must lower to the env read");
        let compiled = compile("$__loc__").expect("$__loc__ must lower");
        let root = compiled.root();
        let crate::program::ProgramNode::Stage {
            start: crate::program::StageStart::Literal(value),
            ..
        } = &compiled.arena()[root.index()]
        else {
            panic!("$__loc__ must lower to a literal stage");
        };
        let jqf_data::Value::Object(object) = value.untagged() else {
            panic!("$__loc__ must be an object");
        };
        let Value::String(file) = object.get("file").map(jqf_data::Value::untagged).expect("file") else {
            panic!("file must be a string");
        };
        assert_eq!(file, "<top-level>");
        assert!(matches!(
            object.get("line").map(jqf_data::Value::untagged),
            Some(Value::Number(_))
        ));
    }

    #[test]
    fn compiles_label_and_break() {
        // The non-local-exit vertical replaced the rejection this test used to
        // assert. `break` resolves against its OWN scope stack, so a label and a
        // variable may share a spelling without either capturing the other.
        for source in [
            "label $out | 1",
            "label $out | (1, break $out, 2)",
            "label $o | label $o | break $o",
            "label $x | (1 as $x | break $x)",
            "[label $o | .[] | break $o]",
        ] {
            assert!(compile(source).is_ok(), "{source} should compile");
        }
    }

    #[test]
    fn every_prelude_definition_is_named_in_the_gate() {
        // The substring gate decides whether the prelude is parsed at all, so a
        // definition missing from `STDLIB_NAMES` would be silently unreachable:
        // its calls would report `not defined` even though the prelude defines
        // it. This pins the two together in the only direction that can break.
        for line in super::STDLIB_PRELUDE.lines() {
            let Some(rest) = line.strip_prefix("def ") else {
                continue;
            };
            let name: &str = rest
                .split(['(', ':'])
                .next()
                .expect("a definition line names a function")
                .trim();
            assert!(
                super::STDLIB_NAMES.contains(&name),
                "prelude defines `{name}` but STDLIB_NAMES omits it, \
                 so the gate would never parse the prelude for it"
            );
        }
    }

    #[test]
    fn every_extension_prelude_definition_is_named_in_the_gate() {
        // The extension prelude rides the same substring gate as the stdlib
        // one, so a definition missing from `EXTENSION_NAMES` would be silently
        // unreachable: its calls would report `not defined`.
        for line in super::EXTENSION_PRELUDE.lines() {
            let Some(rest) = line.strip_prefix("def ") else {
                continue;
            };
            let name: &str = rest
                .split(['(', ':'])
                .next()
                .expect("a definition line names a function")
                .trim();
            assert!(
                super::EXTENSION_NAMES.contains(&name),
                "extension prelude defines `{name}` but EXTENSION_NAMES omits it, \
                 so the gate would never parse the prelude for it"
            );
        }
    }

    #[test]
    fn prelude_enumerated_matches_the_preludes() {
        // `builtins/0` appends `PRELUDE_ENUMERATED` to the registry's own
        // enumeration; the two must describe the SAME surface — the union of
        // the stdlib and extension preludes. Every def either prelude spells
        // must be listed, and every listed name (except the `empty/0` syntax
        // form) must be spelled by one of them.
        let mut prelude = alloc::collections::BTreeSet::new();
        for prelude_text in [super::STDLIB_PRELUDE, super::EXTENSION_PRELUDE] {
            for line in prelude_text.lines() {
                let Some(rest) = line.strip_prefix("def ") else {
                    continue;
                };
                let head = rest.split(':').next().expect("a definition line has a colon");
                let name = head
                    .split(['(', ';'])
                    .next()
                    .expect("a definition line names a function");
                let arity = if head.contains('(') {
                    u8::try_from(head.split('(').nth(1).unwrap_or("").split(';').count()).unwrap_or(u8::MAX)
                } else {
                    0
                };
                prelude.insert((String::from(name), arity));
            }
        }
        for (name, arity) in jqf_builtins::registry::PRELUDE_ENUMERATED {
            // `empty` is a syntax form, not a prelude def; the `~` engine
            // constructors are engine-surface syntax, not prelude defs — both
            // are enumerated for discoverability but live outside the prelude
            // text, so the two-list pin exempts them.
            if (*name, *arity) == ("empty", 0) || name.starts_with('~') {
                continue;
            }
            assert!(
                prelude.contains(&(String::from(*name), *arity)),
                "PRELUDE_ENUMERATED lists {name}/{arity} but the preludes do not define it; prelude={prelude:?}"
            );
        }
        for (name, arity) in &prelude {
            assert!(
                jqf_builtins::registry::PRELUDE_ENUMERATED.contains(&(name.as_str(), *arity)),
                "the preludes define {name}/{arity} but PRELUDE_ENUMERATED omits it"
            );
        }
    }

    #[test]
    fn the_prelude_gate_admits_every_name_it_lists() {
        // The other direction is only a wasted parse, but a stale name is still
        // a maintenance smell worth catching.
        for name in super::STDLIB_NAMES {
            assert!(
                super::STDLIB_PRELUDE.contains(&alloc::format!("def {name}")),
                "STDLIB_NAMES lists `{name}`, which the prelude does not define"
            );
        }
        for name in super::EXTENSION_NAMES {
            assert!(
                super::EXTENSION_PRELUDE.contains(&alloc::format!("def {name}")),
                "EXTENSION_NAMES lists `{name}`, which the prelude does not define"
            );
        }
    }

    #[test]
    fn provable_divergence_compiles_and_is_a_runtime_condition() {
        // The compile-time always-recurses refusal was a FALSE POSITIVE on
        // programs the floor completes without ever evaluating the divergent
        // call (`first(1, def f: f; f)` answers 1), so the check was dropped:
        // a diverging program is a run-time condition now, and the tail-round
        // counter raises the typed depth error instead of hanging
        // (see `a_divergent_recursion_terminates_with_the_depth_error`).
        // Every one of these previously-rejected programs compiles.
        for source in [
            "def f: f; f",
            "def f: (f); f",
            "def f: f, 1; f",
            "def f: f | 1; f",
            "def f: if . then f else f end; f",
            "def f: label $o | f; f",
            "def f(g): f(g); f(.)",
        ] {
            compile(source).unwrap_or_else(|error| panic!("{source:?} must compile: {error:?}"));
        }
    }

    #[test]
    fn a_recursion_with_an_escape_is_not_called_divergent() {
        // The analysis must never claim divergence it cannot prove: each of
        // these has a path that avoids the self-call, so the divergence analysis
        // must NOT refuse them. Each now routes to the CALLABLE path (recursion
        // compiles and runs, bounded at run time) — `empty | f` is the
        // sharpest: the floor TERMINATES on it (exit 0, no output), so calling it
        // divergent would be a straightforward lie.
        for source in [
            "def r: if . > 3 then . else .+1|r end; r",
            "def f: if . then 1 else f end; f",
            "def f: empty | f; f",
            "def f: .[] | f; f",
            "def f: f + 1; f",
            // A PRODUCTIVE generator: `1, f` emits before it recurses, so
            // `[limit(3; f)]` is `[1,1,1]` — the definition terminates for
            // every consumer that stops asking.
            // Calling it non-terminating was a false proof: only the LEFT
            // operand of a comma is guaranteed to be reached, and reaching the
            // right one means the left already finished.
            "def f: 1, f; f",
            "def f: (1, f), 2; f",
        ] {
            assert!(compile(source).is_ok(), "{source} must compile via the callable path");
        }
    }

    #[test]
    fn a_divergent_recursion_terminates_with_the_depth_error() {
        // A genuinely divergent program still terminates with a typed depth
        // error — never a hang and never a native stack overflow. `def f: f; f`
        // is a bare tail self-call, which the trampoline runs flat, so it is the
        // tail-round ceiling that stops it; a non-tail divergence (`[f]`) stops
        // at the callable-depth ceiling. The debug per-level cost of a nested
        // callable is tens of KiB, so the nested arm runs on a big thread
        // exactly as the deep decode/recursion tests do.
        extern crate std;
        std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(|| {
                let programs = ["def f: f; f", "def f: [f]; f"];
                for source in programs {
                    let program = compiled(source);
                    let mut resources = resources();
                    let outcome = jqf_builtins::codec_result::CodecInputOutcome::Result(
                        jqf_builtins::codec_result::EngineResult::owned(Value::Null),
                    );
                    let crate::exec::EngineRun::Stream { mut stream, .. } =
                        program.try_run(outcome, &resources).expect("run seeds")
                    else {
                        panic!("a recursive def is a stream");
                    };
                    let mut raised = false;
                    loop {
                        match stream.poll(&mut resources) {
                            Ok(crate::exec::RunPoll::Complete) => break,
                            Ok(crate::exec::RunPoll::Pending) => {
                                // The fixed 4096-credit meter is exhausted;
                                // open the next cooperative entry the way the
                                // CLI host does.
                                resources.try_begin_next_cooperative_entry(4096).expect("entry");
                            }
                            Ok(crate::exec::RunPoll::Item(_)) => {}
                            Err(crate::exec::EngineRunError::Codec(error)) => {
                                assert!(
                                    matches!(
                                        error.kind(),
                                        jqf_codec_core::CodecFailureKind::Resource(
                                            jqf_resource::ResourceError::RecursionLimit { .. }
                                        )
                                    ),
                                    "{source} must raise the typed depth error, got {error:?}"
                                );
                                raised = true;
                                break;
                            }
                            Err(other) => {
                                panic!("{source} must raise RecursionLimit, got {other:?}")
                            }
                        }
                    }
                    assert!(raised, "{source} must raise, not complete");
                }
            })
            .expect("big-stack thread spawns")
            .join()
            .expect("big-stack thread does not panic");
    }
    #[test]
    fn tail_recursive_def_compiles_with_tail_flag_set() {
        let program = compiled("def f: if . <= 0 then \"done\" else (. - 1) | f end; f");
        let nodes = program.program.nodes();
        let mut tail_count = 0usize;
        for node in nodes {
            if let ProgramNode::CallDef { tail, .. } = node
                && *tail
            {
                tail_count += 1;
            }
        }
        assert!(
            tail_count >= 1,
            "tail-recursive def must have at least one tail=self CallDef, got {tail_count}"
        );
    }

    #[test]
    fn non_tail_recursive_def_leaves_tail_flag_false() {
        let program = compiled("def f: if . <= 0 then 0 else 1 + (. - 1 | f) end; f");
        let nodes = program.program.nodes();
        let mut tail_self = false;
        for node in nodes {
            if let ProgramNode::CallDef { tail, .. } = node
                && *tail
            {
                tail_self = true;
            }
        }
        assert!(!tail_self, "non-tail recursive def must not mark any CallDef as tail");
    }

    #[test]
    fn comma_last_member_self_call_is_marked_tail() {
        // `def f: 1, f` — the self-call is the comma's LAST
        // member, whose outputs are the comma's final outputs, so the
        // trampoline may re-run the body per round instead of nesting one
        // level per emitted output (`limit(5000; f)` must never approach the
        // depth ceiling). The LEFT member is never tail.
        for source in ["def f: 1, f; [limit(3;f)]", "def f: ., ((.+1)|f); [limit(3;f)]"] {
            let program = compiled(source);
            let mut tail_self = false;
            for node in program.program.nodes() {
                if let ProgramNode::CallDef { tail, .. } = node
                    && *tail
                {
                    tail_self = true;
                }
            }
            assert!(tail_self, "{source} must mark its comma-last self-call tail");
        }
    }

    #[test]
    fn pipe_left_self_call_is_marked_tail() {
        // `def nat: 1, (nat|.+1)` — the self-call is the pipe's LEFT member.
        // Not classic tail, but the trampoline is sound there: the body
        // re-evaluates into the same FlatMapBody the call's outputs would
        // enter, and the accumulated frames re-apply each level's `. + 1`
        // exactly once. `limit(700; nat)` must answer, not hit the ceiling.
        let program = compiled("def nat: 1, (nat|.+1); [limit(3;nat)]");
        let mut tail_self = false;
        for node in program.program.nodes() {
            if let ProgramNode::CallDef { tail, .. } = node
                && *tail
            {
                tail_self = true;
            }
        }
        assert!(tail_self, "a pipe-left self-call must be marked for the trampoline");
    }

    #[test]
    fn tail_recursive_def_with_params_compiles() {
        let program = compiled("def f($n): if $n <= 0 then \"done\" else f($n-1) end; f(1000)");
        let nodes = program.program.nodes();
        let mut tail_count = 0usize;
        for node in nodes {
            if let ProgramNode::CallDef { tail, .. } = node
                && *tail
            {
                tail_count += 1;
            }
        }
        assert!(
            tail_count >= 1,
            "tail-recursive def with params must have at least one tail=self CallDef, got {tail_count}"
        );
    }

    #[test]
    fn tail_recursive_filter_parameter_def_marks_tail() {
        let program = compiled("def f(n): f(n); f(1)");
        let nodes = program.program.nodes();
        let mut tail_count = 0usize;
        let mut calldefs = 0usize;
        let mut callfilters = 0usize;
        for node in nodes {
            match node {
                ProgramNode::CallDef { tail, .. } => {
                    calldefs += 1;
                    if *tail {
                        tail_count += 1;
                    }
                }
                ProgramNode::CallFilter { .. } => callfilters += 1,
                _ => {}
            }
        }
        assert!(
            tail_count >= 1,
            "tail-recursive filter-parameter def must mark a tail CallDef; \
             calldefs={calldefs} tail={tail_count} callfilters={callfilters}"
        );
    }
    #[test]
    fn an_unbound_label_is_an_undefined_label_error() {
        // A label's lexical extent is its BODY only, exactly as a binding's is,
        // so a `break` outside it is the `$*label-o is not defined` (exit-3).
        // It is reported on its OWN arm rather than as an undefined VARIABLE:
        // the two namespaces are distinct, and conflating them would misstate
        // which scope stack the lookup missed.
        for source in ["break $o", "(label $o | 1), break $o", "1 as $o | break $o"] {
            match compile(source) {
                Err(EngineCompileError::UndefinedLabel { name, .. }) => {
                    assert_eq!(name, "$o", "{source}");
                }
                other => panic!("expected {source:?} undefined label, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_unbound_variable_is_an_undefined_variable_error() {
        // A binding's lexical extent is its BODY only, so a reference outside it
        // is the compile-time `$x is not defined` (exit-3 class), reported as
        // a resolution failure rather than an out-of-subset rejection.
        for source in ["$x", "(2 as $x | $x), $x", "reduce .[] as $x (0; 0) | $x"] {
            match compile(source) {
                Err(EngineCompileError::UndefinedVariable { name, .. }) => {
                    assert_eq!(name, "$x", "{source}");
                }
                other => panic!("expected {source:?} undefined, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_loop_init_cannot_see_its_own_binding() {
        // The scope discipline: the SOURCE and the INIT are outside the binding
        // (the init sees the outer dot), only UPDATE and EXTRACT are inside it.
        assert!(matches!(
            compile("reduce .[] as $x ($x; .)"),
            Err(EngineCompileError::UndefinedVariable { .. })
        ));
        assert!(matches!(
            compile("reduce $x as $x (0; .)"),
            Err(EngineCompileError::UndefinedVariable { .. })
        ));
    }

    #[test]
    fn an_index_operand_is_lowered_against_the_chain_input() {
        // The frame wraps the WHOLE chain and `Bind`'s source runs with dot
        // unchanged, so `.a[.b]` resolves `.b` against the chain's INPUT. Lowering
        // the operand inside the stage would have read `.a` instead.
        let program = compiled(".a[.b]");
        let nodes = program.program.nodes();
        let ProgramNode::Bind { source, body, .. } = &nodes[program.program.root().index()] else {
            panic!("a general index must lower to a binder frame");
        };
        let ProgramNode::Stage {
            start: StageStart::Current,
            steps: operand,
        } = &nodes[source.index()]
        else {
            panic!("the operand must start from the chain's own input");
        };
        assert!(matches!(operand[0].access(), StepAccess::Key(key) if key == "b"));
        let ProgramNode::Stage { steps: chain, .. } = &nodes[body.index()] else {
            panic!("the frame must wrap the chain's stage");
        };
        assert!(matches!(chain[0].access(), StepAccess::Key(key) if key == "a"));
        assert!(matches!(chain[1].access(), StepAccess::DynVar(_)));
    }

    #[test]
    fn the_last_bracket_is_the_outermost_binder() {
        // The fork-creation order: the LATER a generator's forks are created the
        // faster it varies, so in `.[.i][.j]` the last bracket is the OUTER loop
        // (`[.[0,1][0,1]]` on `[[10,11],[20,21]]` is `[10,20,11,21]`). Structurally
        // that is the root frame owning the LAST step's slot.
        let program = compiled(".[.i][.j]");
        let nodes = program.program.nodes();
        let ProgramNode::Bind { slot: outer, body, .. } = &nodes[program.program.root().index()] else {
            panic!("two general indexes must nest two binder frames");
        };
        let ProgramNode::Bind {
            slot: inner,
            body: stage,
            ..
        } = &nodes[body.index()]
        else {
            panic!("the outer frame must wrap the inner one");
        };
        let ProgramNode::Stage { steps, .. } = &nodes[stage.index()] else {
            panic!("the inner frame must wrap the stage");
        };
        assert!(matches!(steps[0].access(), StepAccess::DynVar(bound) if bound == inner));
        assert!(matches!(steps[1].access(), StepAccess::DynVar(bound) if bound == outer));
    }

    #[test]
    fn a_term_suppression_barrier_encloses_the_binders_it_covers() {
        // A TERM-level `?` covers the index operand's own error
        // (`[(.[error("x")])?]` is `[]`) while a per-component `?` does not
        // (`[.[error("x")]?]` raises, `[.[error("x")]??]` is `[]`). So the frame the
        // barrier's PREFIX accumulated is flushed INSIDE the `try` body.
        for source in ["(.[.a])?", ".[.a]??"] {
            let program = compiled(source);
            let nodes = program.program.nodes();
            let ProgramNode::Try { body, handler: None } = &nodes[program.program.root().index()] else {
                panic!("{source} must lower to a catchless try");
            };
            assert!(
                matches!(&nodes[body.index()], ProgramNode::Bind { .. }),
                "{source} must keep its binder frame inside the barrier"
            );
        }
    }

    /// The container path of a program's ungated projected plan, rendered as the
    /// receipt spelling (`.` for the root container, `.a.b` / `.a[0]` otherwise).
    #[test]
    fn the_boundary_is_named_inside_a_reduce_or_foreach_source() {
        // The leading shape: `PushdownSplit` could not name a `.[]` inside a
        // loop SOURCE. It can now — the prefix is `.catalog`, the boundary is
        // the `Each` after it, and the loop is the residual consumer of the
        // elements.
        for source in [
            "reduce .catalog[] as $x (0; . + 1)",
            "foreach .catalog[] as $x (0; . + 1)",
        ] {
            assert_eq!(
                compiled(source).element_boundary_consumer(),
                Some(BoundaryConsumer::Fold),
                "{source}"
            );
        }
        // A bound source (`SRC as $x | BODY`) is the binding flavor of the same
        // shape.
        assert_eq!(
            compiled("[.catalog[] as $x | $x.id]").element_boundary_consumer(),
            Some(BoundaryConsumer::Binding)
        );
    }

    #[test]
    fn the_boundary_is_named_through_maps_lowered_body() {
        // `map(f)` lowers to `[.[] | f]`, so its `.[]` lives inside a
        // `CollectArray` body — the second place the pushdown prefix never
        // reaches. The container path splits across the pipe: `.catalog` comes
        // from the FlatMap upstream, the `.[]` from the collect body's stage.
        assert_eq!(
            compiled(".catalog | map(.name) | length").element_boundary_consumer(),
            Some(BoundaryConsumer::Collect)
        );
        assert_eq!(
            compiled("[.catalog[]] | length").element_boundary_consumer(),
            Some(BoundaryConsumer::Collect)
        );
    }

    #[test]
    fn a_non_document_or_non_static_each_names_no_boundary() {
        // A literal-start producer's `.[]` is not a DOCUMENT element stream
        // (the earlier walk recorded one here).
        assert_eq!(compiled("3[]?").element_boundary_consumer(), None);
        // Born-P2 vocabulary before the `.[]` leaves no static container path.
        assert_eq!(compiled("reduce .. as $x (0; . + 1)").element_boundary_consumer(), None);
        assert_eq!(compiled("..[]").element_boundary_consumer(), None);
        // A comma has two members and therefore no single boundary.
        assert_eq!(compiled(".a[], .b[]").element_boundary_consumer(), None);
        // No `.[]` at all.
        assert_eq!(compiled(".catalog[0].id").element_boundary_consumer(), None);
    }

    #[test]
    fn every_receipt_table_row_has_a_nameable_boundary() {
        // Every row: each one NAMES its element boundary — `.catalog` in every
        // case — and records what consumes the elements.
        let table = [
            ("reduce .catalog[] as $x (0; . + 1)", BoundaryConsumer::Fold),
            ("[.catalog[]] | length", BoundaryConsumer::Collect),
            (".catalog | map(.name) | length", BoundaryConsumer::Collect),
            ("reduce .catalog[].id as $i (0; . + $i)", BoundaryConsumer::Fold),
            ("[.catalog[] | select(.id > 35990)] | length", BoundaryConsumer::Collect),
            ("[.catalog[] | select(.id > 35990)]", BoundaryConsumer::Collect),
            ("[.catalog[] | length]", BoundaryConsumer::Collect),
        ];
        for (source, consumer) in table {
            let program = compiled(source);
            assert_eq!(
                program.element_boundary_consumer(),
                Some(consumer),
                "{source} must name a boundary with the {consumer:?} consumer"
            );
        }
    }

    #[test]
    fn a_bind_or_loop_source_pushes_its_static_path_down() {
        // The pushdown spine now enters a `Bind`/`Reduce`/`Foreach` SOURCE when
        // every other outer-dot reader is document-independent AND the codec can
        // resolve the source completely. The requirement must then be
        // byte-identical to the bare source path's.
        let resources = resources();
        for (source, bare) in [
            (".catalog[500].id as $i | [$i, $i*2]", ".catalog[500].id"),
            (".catalog[0] as $x | $x.id", ".catalog[0]"),
            ("reduce .meta.n as $n (0; . + $n)", ".meta.n"),
            ("foreach .meta.n as $n (0; . + $n)", ".meta.n"),
            (".a.b as $x | {v: $x}", ".a.b"),
        ] {
            let pushed = compiled(source)
                .try_requirement(&resources)
                .expect("bind-source requirement");
            let plain = compiled(bare).try_requirement(&resources).expect("bare requirement");
            assert_eq!(pushed.result(), AccessResultKind::Located, "{source}");
            assert_eq!(
                pushed.footprint().fingerprint(),
                plain.footprint().fingerprint(),
                "{source} must lower {bare}'s footprint"
            );
        }

        // The three declining laws, each for its own reason.
        for source in [
            // The binder's BODY reads the outer dot, which the located value is not.
            ".catalog[500].id as $i | .meta",
            // The loop's INIT reads the outer dot.
            "reduce .meta.n as $n (.meta.n; . + $n)",
            // The source FANS OUT over the located container rather than
            // resolving completely.
            "reduce .catalog[].id as $i (0; . + $i)",
            "reduce .catalog[] as $x (0; . + 1)",
            ".catalog[] as $x | [$x]",
            // A non-static step in the source.
            ".catalog[0].id as $i | (.catalog[$i:2] as $x | [$x])",
            ".catalog[0:2] as $x | [$x]",
        ] {
            assert_eq!(
                compiled(source)
                    .try_requirement(&resources)
                    .expect("requirement")
                    .result(),
                AccessResultKind::CompleteDocument,
                "{source} must keep the whole-document route"
            );
        }
    }

    #[test]
    fn the_range_locate_row_is_the_bare_slice_publish_and_nothing_else() {
        // The row: a bare `Stage` of static Key/Index steps ending in exactly
        // ONE non-optional slice, with nothing after it.
        for source in [
            ".catalog[100:110]",
            ".catalog[:10]",
            ".catalog[10:]",
            ".[1:3]",
            ".a.b[1:3]",
            ".nested[1][0:2]",
            ".catalog[1.7:3.2]",
            // Negative bounds resolve against the observed length in the codec
            // so a tail slice is a row like any other.
            ".catalog[-3:]",
            ".catalog[:-2]",
            ".catalog[-5:-1]",
            // An optional PREFIX step is fine: it only changes the outcome on a
            // path the rung declines anyway.
            ".catalog?[1:3]",
        ] {
            assert!(
                compiled(source).range_locate_eligible(),
                "{source} must be range-locate eligible"
            );
        }
        for (source, why) in [
            // One trailing range; stacks decline.
            (".catalog[0:4][1:3]", "a slice STACK"),
            // Nothing may follow the slice — the codec's exact path cannot carry
            // it and the rung runs no residual.
            (".catalog[1:3][0]", "a step after the slice"),
            (".catalog[1:3].name", "a key after the slice"),
            (".catalog[1:3] | length", "the range-COUNT row, which is a pipe"),
            // Prefix-scoping: an element boundary, a descent, or a dynamic index
            // before the slice is not a static exact path.
            (".catalog[][0:2]", "an `.[]` before the slice"),
            ("..[0:2]", "a descent before the slice"),
            ("0 as $i | .catalog[$i][0:2]", "a `$var` index before the slice"),
            // The classifications must not move.
            ("[.catalog[] | .tags[1:3]]", "a per-ELEMENT slice"),
            ("[.catalog[1:3][].name]", "a range on a CONTAINER path"),
            // The slice's own `?` is a different program; the floor serves it.
            (".catalog[1:3]?", "the slice step's own `?`"),
            // The boundary law's declines carry straight through.
            ("1 as $x | .catalog[$x:3]", "a `$var` BOUND"),
            // Not a bare `Stage` root at all.
            ("[.catalog[1:3]]", "a construction barrier"),
            (".catalog[1:3], .meta", "a comma"),
        ] {
            assert!(
                !compiled(source).range_locate_eligible(),
                "{source} must decline the range-locate rung ({why})"
            );
        }
    }

    #[test]
    fn naming_a_boundary_is_still_not_eligibility_by_itself() {
        // The rung was widened, and NAMING a boundary is still not the same as
        // taking it: every shape below names one and declines, each for a law
        // of its own (clause 3, or an absent shape row).
        //
        // The SPLIT-path restriction is no longer among those laws.
        // `.catalog | map(.name) | length` names its container across a pipe and
        // now streams; what its split path costs is the right to INTERPRET a
        // container-negative outcome, which the split path withholds and
        // `split_container_paths_stream_but_decline_the_container` pins.
        for source in [
            "reduce .catalog[] as $x (0; . + 1)",
            "foreach .catalog[] as $x (0; . + 1)",
            "[.catalog[] as $x | $x.id]",
        ] {
            let program = compiled(source);
            assert!(
                program.element_boundary_consumer().is_some(),
                "{source} must name a boundary"
            );
        }
    }

    #[test]
    fn parse_error_is_surfaced() {
        assert!(matches!(compile(".["), Err(EngineCompileError::Parse(_))));
    }

    #[test]
    fn the_correlated_joins_are_recognized_end_to_end() {
        // The recognition table is unit-tested against hand-built arenas; this
        // test pins it to the LOWERING, so a change to how `map`, `select`, or
        // `as` lowers cannot silently empty the table.
        for (source, rows) in [
            (
                ".users[0:100] as $users | .orders as $orders | $users \
                 | map(. as $u | {id: $u.id, order_count: \
                 ($orders | map(select(.user_id == $u.id)) | length)})",
                1,
            ),
            (
                ".orders as $orders | .orders[0:200] | map(. as $a \
                 | {id: $a.id, siblings: ([$orders[] \
                 | select(.user_id == $a.user_id and .channel == $a.channel)] | length)})",
                1,
            ),
            (
                ".orders as $orders | .users[0:1000] | map(. as $u \
                 | {id: $u.id, matching: ([$orders[] | select(.user_id == $u.id \
                 and .channel == (if $u.active then \"web\" else \"partner\" end))] \
                 | length)})",
                1,
            ),
            (
                ".users[0:500] as $users | .orders as $orders | .events as $events | $users \
                 | map(. as $u | {id: $u.id, email: $u.email} \
                 + {orders: ($orders | map(select(.user_id == $u.id)) | length), \
                 recent_events: ($events \
                 | map(select(.user_id == $u.id and .created_day < 30)) | length)})",
                2,
            ),
            // The K2 LITERAL probe, end to end. The row's own unit test builds
            // its arena by hand, and every row above probes through a `$var`,
            // so nothing here saw the shape until the spine hoist started
            // rewriting `Binary` nodes: `select(.id == 3)` would become
            // `.id | (. == 3)` and the row would vanish silently, with output
            // unchanged and the Θ(k·m) rescan back.
            (".orders as $o | [$o[] | select(.id == 3)] | length", 1),
            (".orders as $o | $o | map(select(.status == \"paid\")) | length", 1),
        ] {
            let program = compiled(source);
            assert_eq!(program.program.scans().len(), rows, "correlated-scan rows for {source}");
        }
    }

    #[test]
    fn the_near_miss_join_shapes_stay_out_of_the_table() {
        // Shapes that LOOK correlated and are deliberate non-rows. (`INDEX/2`,
        // the other named non-row, is not a registered builtin here at all, so
        // the table cannot see it even in principle.)
        for source in [
            // The anti-join: expands through `all/2`'s label machinery.
            ".orders as $orders | [.users[0:1000][] | . as $u \
             | select(all($orders[]; .user_id != $u.id)) | .id]",
            // Not an equality.
            ".orders as $orders | .users[] | . as $u \
             | [$orders[] | select(.user_id != $u.id)] | length",
            // The leftmost conjunct of an `or` is not a necessary condition.
            ".orders as $orders | .users[] | . as $u \
             | [$orders[] | select(.user_id == $u.id or .other == $u.id)] | length",
            // Both sides read the current element: nothing is element-independent.
            ".orders as $orders | .users[] | . as $u \
             | [$orders[] | select(.user_id == .id)] | length",
            // A residual after the iteration: the value reaching `select` is not
            // the child, so the index's key path is not the predicate's.
            ".orders as $orders | .users[] | . as $u \
             | [$orders[].line | select(.user_id == $u.id)] | length",
        ] {
            assert!(compiled(source).program.scans().is_empty(), "must decline: {source}");
        }
        // The former "unbound source" near-miss — `.users[] | . as $u |
        // [.orders[] | select(.user_id == $u.id)]` — became a ROW with the
        // generalized source recognition: its inner scan is a `Current`-start
        // source (`[orders, Each]`), and the runtime warm-up keys the index
        // slot on the CONTAINER'S NODE HANDLE, so a container that is missing
        // (the scan declines to the naive raise), owned, or re-created per
        // outer iteration never builds an index it cannot amortize.
        let binding = compiled(".users[] | . as $u | [.orders[] | select(.user_id == $u.id)] | length");
        let scans = binding.program.scans();
        assert_eq!(scans.len(), 1, "the unbound-source spelling is a row now");
    }
}

/// Regression guard: the count and element demands are compile-time
/// constants of the immutable program, so the per-record consultation pattern
/// of the sequence drive must never re-derive them. The derivation counters
/// it reads live at the derivation sites (the accessors on the unfixed code,
/// the constructors after the cache lands), so the test fails on the unfixed
/// code and pins the cache against a future regression.
#[cfg(test)]
mod demand_cache {
    use super::*;
    use core::sync::atomic::Ordering;

    fn resources() -> ResourceContext<'static> {
        let account = jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u32::MAX,
        ))
        .expect("account");
        let work = jqf_resource::WorkMeter::try_new_v1(4096).expect("work");
        jqf_resource::ResourceContext::new(account, &jqf_resource::ContinueControl, work).expect("resources")
    }

    #[test]
    fn demands_are_derived_once_per_program_not_per_record() {
        // The sequence drive's per-record pattern: `count_answer` consults
        // the count demand on every record, `element_answer` consults the
        // element demand when the count declines, and the residual run probes
        // both again when both fast paths decline. A multi-record input
        // therefore consults each accessor several times per record; none of
        // those consultations may re-run the derivation.
        let resources = resources();
        let policy = crate::codec_requirement::CodecRequirementPolicy::new(
            jqf_codec_core::ValidationMode::Strict,
            jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
        );
        let compiled = try_compile_program("[.catalog[] | .id] | length", policy, &resources).expect("compiles");
        // Hold the derivation lock across the whole snapshot-to-assertion
        // window: a concurrent test's compile must not move the counters
        // mid-assertion (the counters are shared across the test binary).
        let guard = super::DERIVATION_LOCK.lock().expect("derivation lock");
        let count_before = super::COUNT_DEMAND_DERIVATIONS.load(Ordering::Relaxed);
        let element_before = super::ELEMENT_DEMAND_DERIVATIONS.load(Ordering::Relaxed);
        for _ in 0..64 {
            // count_answer's consultation.
            let _ = compiled.count_demand();
            // element_answer's consultation (count declined).
            let _ = compiled.element_demand();
            // The residual run's decline probes (both fast paths declined).
            let _ = compiled.count_demand().is_some();
            let _ = compiled.element_demand().is_some();
        }
        assert_eq!(
            super::COUNT_DEMAND_DERIVATIONS.load(Ordering::Relaxed),
            count_before,
            "the count demand re-derived per record consultation"
        );
        assert_eq!(
            super::ELEMENT_DEMAND_DERIVATIONS.load(Ordering::Relaxed),
            element_before,
            "the element demand re-derived per record consultation"
        );
        drop(guard);
    }
}

#[cfg(test)]
mod r2_type_probe_tests {
    use super::*;

    fn ledger() -> ResourceContext<'static> {
        let account = jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u32::MAX,
        ))
        .expect("account");
        let work = jqf_resource::WorkMeter::try_new_v1(4096).expect("work");
        jqf_resource::ResourceContext::new(account, &jqf_resource::ContinueControl, work).expect("resources")
    }

    #[test]
    fn type_lowers_whole_document_with_the_hint() {
        let resources = ledger();
        let policy = crate::codec_requirement::CodecRequirementPolicy::new(
            jqf_codec_core::ValidationMode::Strict,
            jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
        );
        let program = try_compile_program("type", policy, &resources).expect("compiles");
        assert!(
            program.program.split().is_whole_document(),
            "type must be whole-document split"
        );
        assert!(program.type_demand(), "type must carry the type demand");
        let requirement = program.try_requirement(&resources).expect("lowers");
        assert!(requirement.footprint().is_whole(), "type must lower whole");
        assert!(requirement.type_demand(), "requirement must carry the hint");
    }

    #[test]
    fn histogram_fold_has_a_prune_and_attaches_it() {
        let resources = ledger();
        let policy = crate::codec_requirement::CodecRequirementPolicy::new(
            jqf_codec_core::ValidationMode::Strict,
            jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
        );
        let source = "reduce (.catalog[] | .attrs.warehouse) as $w ({}; .[$w] += 1)";
        let program = try_compile_program(source, policy, &resources).expect("compiles");
        assert!(
            program.program.split().is_whole_document(),
            "the fold must be whole-document"
        );
        assert!(
            program.prune_tree().is_some(),
            "the fold must derive a kept-subtree tree"
        );
        let requirement = program.try_requirement(&resources).expect("lowers");
        assert!(requirement.prune().is_some(), "the requirement must carry the prune");
        // Unbounded element rows decode EAGER so the prune is armed (JSON
        // drops prune under a nonzero frontier). Bounded prefixes stay lazy.
        assert_eq!(requirement.lazy_frontier(), 0, "the unbounded fold is eager");
        assert!(
            requirement.element().is_some(),
            "the fold must carry the element demand hint"
        );
    }

    #[test]
    fn type_question_declines_and_path_type_exact_locates() {
        let resources = ledger();
        let policy = crate::codec_requirement::CodecRequirementPolicy::new(
            jqf_codec_core::ValidationMode::Strict,
            jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
        );
        for source in ["type?", "[.a] | type", "type, ."] {
            let program = try_compile_program(source, policy, &resources).expect("compiles");
            assert!(!program.type_demand(), "{source} must not be a type row");
        }
        let path_type = try_compile_program(".a | type", policy, &resources).expect("compiles");
        assert!(path_type.type_demand(), "PATH | type is a type row");
        assert_eq!(
            path_type.type_demand_path().expect("path"),
            alloc::vec![jqf_data::CountStep::ObjectKey(alloc::string::String::from("a"))]
        );
        let requirement = path_type.try_requirement(&resources).expect("lowers");
        assert!(
            !requirement.footprint().is_whole(),
            "PATH | type Exact-locates the prefix so residual type sees the named node"
        );
        assert!(requirement.type_demand(), "PATH | type still carries the kind hint");
    }

    #[test]
    fn slice_then_type_does_not_lower_as_bare_root_type() {
        let resources = ledger();
        let policy = crate::codec_requirement::CodecRequirementPolicy::new(
            jqf_codec_core::ValidationMode::Strict,
            jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
        );
        let program = try_compile_program(".[1:3] | type", policy, &resources).expect("compiles");
        assert!(
            !program.type_demand(),
            "a trailing slice is not a type row: empty-path plus range would type the root"
        );
        let requirement = program.try_requirement(&resources).expect("lowers");
        assert!(
            !requirement.type_demand(),
            ".[1:3] | type must not take the Whole+empty-path type arm"
        );
        let users = try_compile_program(".users[1:3] | type", policy, &resources).expect("compiles");
        let users_requirement = users.try_requirement(&resources).expect("lowers");
        assert!(
            !users_requirement.footprint().is_whole(),
            ".users[1:3] | type Exact-locates the prefix"
        );
    }

    /// Engine-lowered Exact `type_demand` at `.users` must not retain the array's child nodes.
    #[test]
    fn exact_type_demand_at_users_does_not_retain_child_nodes() {
        let mut resources = ledger();
        let policy = crate::codec_requirement::CodecRequirementPolicy::new(
            jqf_codec_core::ValidationMode::Strict,
            jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
        );
        let program = try_compile_program(".users | type", policy, &resources).expect("compiles");
        let requirement = program.try_requirement(&resources).expect("lowers");
        assert!(requirement.type_demand(), "PATH | type carries the kind hint");
        assert!(
            !requirement.footprint().is_whole(),
            "PATH | type Exact-locates the prefix"
        );
        let bytes = br#"{"users":[1,2,3]}"#;
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(0), jqf_source::SourceKind::Input),
            "input.json",
            bytes,
            0,
        );
        let registration = jqf_codec_json::registration().expect("json");
        let mut provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(
                source,
                jqf_codec_core::DecodeRequest {
                    validation: jqf_codec_core::ValidationMode::Strict,
                    diagnostics: jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
                    dialect: &jqf_data::DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: jqf_codec_json::VALUE_SEPARATORS,
                },
                &mut resources,
            )
            .expect("provider");
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let result = {
            let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
            run.set_cooperative_credits(4_096);
            session.decode(&mut run).expect("decode")
        };
        let jqf_codec_core::AccessOutcome::Located(located) = result.outcome() else {
            panic!("PATH | type must locate")
        };
        let jqf_codec_core::ExactSelectionRecord::Node { node, .. } = located.result() else {
            panic!("located node")
        };
        let document = located.product().document();
        let view = document.value_view(*node).expect("view");
        assert_eq!(
            view.kind().expect("kind"),
            jqf_data::ValueKind::Array,
            ".users is an array"
        );
        let array = view.array().expect("array").expect("array view");
        assert_eq!(array.len(), 0, "type_demand must not retain child nodes");
        assert_eq!(document.node_count(), 1, "kind-only document is the empty array");
    }
}

#[cfg(test)]
mod r6_element_requirement_probe {
    use super::*;

    #[test]
    fn element_requirement_carries_the_hint() {
        let resources = ResourceContext::new(
            jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u32::MAX,
            ))
            .expect("account"),
            &jqf_resource::ContinueControl,
            jqf_resource::WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("resources");
        let policy = crate::codec_requirement::CodecRequirementPolicy::new(
            jqf_codec_core::ValidationMode::Strict,
            jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
        );
        for (source, should, eager) in [
            (".catalog[] | .name", true, false),
            (".catalog[] | {id,name,sku}", true, false),
            (".[] | length", true, false),
            (".catalog | map(.name)", true, false),
            (".catalog | map({id, name})", true, false),
            ("[.catalog[] | {id}]", true, false),
            (
                "reduce (.catalog[] | .attrs.warehouse) as $w ({}; .[$w] += 1)",
                true,
                true,
            ),
            (".catalog[] | select(.id > 1) | .name", true, false),
            (".catalog | map(select(.id > 1))", true, false),
            (".", false, false),
        ] {
            let program = try_compile_program(source, policy, &resources).expect("compiles");
            let demand = program.element_demand();
            assert_eq!(demand.is_some(), should, "{source}");
            let requirement = program.try_requirement(&resources).expect("lowers");
            assert_eq!(requirement.element().is_some(), should, "{source} hint");
            if should {
                assert!(
                    requirement.footprint().is_whole(),
                    "{source} must lower the whole document"
                );
                assert_eq!(
                    requirement.lazy_frontier(),
                    u32::from(!eager),
                    "{source} fan-out stays lazy; unbounded fold is eager"
                );
            }
        }
    }

    #[test]
    fn unbounded_count_and_collect_are_eager_bounded_element_stays_lazy() {
        let resources = ResourceContext::new(
            jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u32::MAX,
            ))
            .expect("account"),
            &jqf_resource::ContinueControl,
            jqf_resource::WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("resources");
        let policy = crate::codec_requirement::CodecRequirementPolicy::new(
            jqf_codec_core::ValidationMode::Strict,
            jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
        );
        let count = try_compile_program(".catalog | length", policy, &resources).expect("compiles");
        assert!(count.count_demand().is_some(), "PATH|length is a count row");
        assert_eq!(
            count.try_requirement(&resources).expect("lowers").lazy_frontier(),
            0,
            "unbounded count is eager"
        );
        let collected = try_compile_program("[.catalog[] | .id]", policy, &resources).expect("compiles");
        assert!(
            collected.element_demand().is_some(),
            "collected fan-out is an element row"
        );
        assert_eq!(
            collected.try_requirement(&resources).expect("lowers").lazy_frontier(),
            1,
            "unbounded collect stays lazy so the span leaf can extract"
        );
        let limited = try_compile_program("limit(2; .catalog[] | .id)", policy, &resources).expect("compiles");
        assert!(limited.element_demand().is_some(), "limit fan-out is an element row");
        assert_eq!(
            limited.try_requirement(&resources).expect("lowers").lazy_frontier(),
            1,
            "bounded prefix stays lazy"
        );
    }

    #[test]
    fn collect_piped_to_add_rewrites_to_reduce() {
        let resources = ResourceContext::new(
            jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u32::MAX,
            ))
            .expect("account"),
            &jqf_resource::ContinueControl,
            jqf_resource::WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("resources");
        let policy = crate::codec_requirement::CodecRequirementPolicy::new(
            jqf_codec_core::ValidationMode::Strict,
            jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
        );
        let program = try_compile_program("[.catalog[] | .n] | add", policy, &resources).expect("compiles");
        assert!(
            matches!(program.arena()[program.root().index()], ProgramNode::Reduce { .. }),
            "[STREAM]|add must be a reduce"
        );
    }

    #[test]
    fn tag_accessor_inserts_intrinsic_tag_clause() {
        let resources = ResourceContext::new(
            jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u32::MAX,
            ))
            .expect("account"),
            &jqf_resource::ContinueControl,
            jqf_resource::WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("resources");
        let policy = crate::codec_requirement::CodecRequirementPolicy::new(
            jqf_codec_core::ValidationMode::Strict,
            jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
        );
        let tagged = try_compile_program(".@tag", policy, &resources).expect("compiles");
        let demand = tagged.try_requirement(&resources).expect("lowers");
        assert!(
            demand
                .demand()
                .clauses()
                .iter()
                .any(|clause| matches!(clause, jqf_codec_core::DemandClause::IntrinsicTag)),
            ".@tag names the intrinsic-tag clause"
        );
        let commented = try_compile_program(".@comment", policy, &resources).expect("compiles");
        let demand = commented.try_requirement(&resources).expect("lowers");
        assert!(
            demand.demand().clauses().iter().any(|clause| matches!(
                clause,
                jqf_codec_core::DemandClause::AttachedFact { role, .. } if role.as_str() == "comment"
            )),
            ".@comment names the comment attached-fact clause"
        );
        let aliased = try_compile_program(".@comment_head", policy, &resources).expect("compiles");
        let demand = aliased.try_requirement(&resources).expect("lowers");
        assert!(
            demand.demand().clauses().iter().any(|clause| matches!(
                clause,
                jqf_codec_core::DemandClause::AttachedFact { role, .. } if role.as_str() == "comment"
            )),
            ".@comment_head lowers as the comment attached-fact clause"
        );
        let href = try_compile_program(".&href", policy, &resources).expect("compiles");
        let demand = href.try_requirement(&resources).expect("lowers");
        assert!(
            demand.demand().clauses().iter().any(|clause| matches!(
                clause,
                jqf_codec_core::DemandClause::Attribute(name) if name.local_name() == "href"
            )),
            ".&href names the attribute clause"
        );
        let plain = try_compile_program(".", policy, &resources).expect("compiles");
        let demand = plain.try_requirement(&resources).expect("lowers");
        assert!(
            demand.demand().clauses().iter().all(|clause| !matches!(
                clause,
                jqf_codec_core::DemandClause::IntrinsicTag
                    | jqf_codec_core::DemandClause::AttachedFact { .. }
                    | jqf_codec_core::DemandClause::Attribute(_)
            )),
            "identity does not demand tag, fact, or attribute clauses"
        );
        let xpath = try_compile_program("xpath(\"//item\")", policy, &resources).expect("compiles");
        let demand = xpath.try_requirement(&resources).expect("lowers");
        assert!(
            demand
                .demand()
                .clauses()
                .iter()
                .any(|clause| matches!(clause, jqf_codec_core::DemandClause::Topology(_))),
            "xpath names topology"
        );
        assert!(
            demand.demand().clauses().iter().any(|clause| matches!(
                clause,
                jqf_codec_core::DemandClause::AttachedFact { role, .. } if role.as_str() == "content"
            )),
            "xpath names content fact"
        );
        assert!(
            try_compile_program("keys", policy, &resources)
                .expect("compiles")
                .keys_demand()
                .is_some()
        );
        assert_eq!(
            try_compile_program(".users | keys", policy, &resources)
                .expect("compiles")
                .keys_demand()
                .expect("path")
                .len(),
            1
        );
    }
}
