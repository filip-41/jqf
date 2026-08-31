//! The opaque compiled program: finish commits the job, accessors report it.
//!
//! [`CompiledProgram::finish`] packs one [`AccessPlan`], one [`Shortcut`],
//! and [`HostIo`]. Codecs charge the plan; execute matches the shortcut; the
//! host matches [`HostIo`]. Requirement charging lives in [`super::access`];
//! document oracle answers live in [`crate::exec::oracles`]. Marks and the
//! slurp rewrite live in [`super::transform`]. Prune is not a shortcut.

#[allow(clippy::wildcard_imports)]
use super::*;

/// Host I/O the compiled job may ask for. Execute does not hold the source
/// window; the SDK matches this before the floor decode.
///
/// Identity echo needs the retained-byte window. Range-locate needs the codec
/// session and byte window. Both stay host I/O; a declined echo or span cut
/// still runs the graph through [`CompiledProgram::execute`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostIo {
    /// Decode, execute, encode.
    Run,
    /// Identity: host may reprint retained bytes.
    Echo,
    /// Range-locate: host may cut a codec span.
    SpanCut,
}

/// A compiled program: prelude-gated, lowered, transformed, analyzed, and
/// finished — ready to lower into a codec requirement and interpret run
/// outcomes.
///
/// The program is opaque: callers observe it through [`try_requirement`],
/// [`execute`], and [`host_io`]. Internally it carries the engine `Program`
/// arena (in path-normal form — a bare `Stage`, or a `Choice`/`FlatMap` graph
/// a comma blocked from fusing), the packed [`AccessPlan`], one [`Shortcut`],
/// and [`HostIo`]. [`try_requirement`] instantiates that plan on this
/// request's ledger; [`execute`] runs the job.
///
/// [`try_requirement`]: CompiledProgram::try_requirement
/// [`execute`]: CompiledProgram::execute
/// [`host_io`]: CompiledProgram::host_io
///
/// `Send + Sync`: a morsel worker borrows the parent program. Retained
/// bytes stay on the parent ledger.
#[derive(Debug)]
pub struct CompiledProgram {
    pub(crate) program: Program,
    pub(crate) policy: CodecRequirementPolicy,
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
    /// The one job finish committed. Accessors match this; they do not keep a
    /// parallel bag of `Option` fields.
    pub(crate) shortcut: Shortcut,
    /// Packed codec access. [`Self::try_requirement`] instantiates this on the
    /// request ledger; it does not re-walk demands.
    pub(crate) plan: AccessPlan,
    /// Host I/O this job may ask for. Packed from the shortcut at finish.
    host: HostIo,
    /// Packed at finish for [`AccessPlan::pack`]; the public accessor returns
    /// this instead of re-walking the arena.
    consumes_whole_document: bool,
    /// Kept-subtree selection over a pulled `input`/`inputs` record.
    /// Present when the program pulls and the pulled demand bounds below
    /// the whole record. The record `NullFirst` drive attaches it as a
    /// per-record hint; the root prune tree stays declined.
    pub(crate) pulled_record: Option<crate::analysis::PruneNode>,
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    // Workers borrow the parent program. A
    // request-local account inside this type would put that account on a
    // worker thread; do not weaken detach to paper over it.
    assert_send_sync::<CompiledProgram>();
};

impl CompiledProgram {
    /// The kept-subtree selection packed onto the access plan (reanchored
    /// when the plan Exact-locates a prefix).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn prune_tree(&self) -> Option<&crate::analysis::PruneNode> {
        self.plan.prune_tree()
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

    /// The split-expansion `$index` slot recorded at compile time, when present.
    #[must_use]
    pub fn runtime_index_slot(&self) -> Option<u32> {
        self.runtime_index_slot
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

    /// Whether this program's root evaluation consumes the entire input
    /// document — the eager side (see [`crate::analysis::lazy`]).
    ///
    /// Packed at finish for [`AccessPlan::pack`]. This accessor returns that
    /// stored fact; it does not re-walk the arena.
    ///
    /// The span-backed lazy representation pays extra time on
    /// touch-everything programs (identity, `..`, root-element folds,
    /// whole-document `|=`), so the CLI keeps those on the eager decode and
    /// offers the lazy default to everything else. Conservative: a program
    /// the analysis cannot prove partial is classified consuming.
    #[must_use]
    pub const fn consumes_whole_document(&self) -> bool {
        self.consumes_whole_document
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
    /// The returned field names borrow this program's arena, so the class
    /// cannot be stored on [`Self`] without a second owned copy that can
    /// drift. This accessor re-walks. Finish walks it once to derive count
    /// demand; that walk is not this accessor.
    #[must_use]
    pub fn projection_class(&self) -> ProjectionClass<'_> {
        self.program.projection_class()
    }

    /// Whether this program NAMES an element boundary, and what consumes its
    /// elements — the analysis fact.
    ///
    /// A named boundary is strictly wider than any single rung's eligibility:
    /// it reaches inside a `reduce`/`foreach` SOURCE and through `map`'s lowered
    /// `[.[] | f]` body, places the pushdown prefix never enters. Naming
    /// changes no routing.
    #[must_use]
    pub fn element_boundary_consumer(&self) -> Option<BoundaryConsumer> {
        Some(self.program.element_boundary()?.consumer())
    }

    /// One construction epilogue: everything a compiled program needs
    /// after [`Program::new`], owned here so both compile entries — the
    /// ordinary lowering and the arena-level slurp-aggregate rewrite —
    /// cannot drift apart. Finish commits one [`super::Shortcut`], the packed
    /// [`AccessPlan`], and [`HostIo`]; per-record consumers read those.
    ///
    /// Prune law, resolved deliberately: BOTH trees are derived
    /// UNCONDITIONALLY and [`crate::analysis::prune_trees`] declines on its
    /// own walk conditions. The rewrite path historically gated the root tree
    /// on `split.is_whole_document()`; that gate was a no-op there — the
    /// rewrite's root is always `reduce inputs as $x (…)`, whose source pull
    /// makes the prune walker record an input pull, and `prune_trees`
    /// blanket-declines any program that pulls (`analysis/prune.rs`). One
    /// law for both callers: derive, and let the analysis decline. The packed
    /// plan stores the (reanchored) tree `try_requirement` attaches; the
    /// analysis-root tree is not kept beside it.
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
        let class = program.projection_class();
        let count = crate::analysis::count::count_demand(program.nodes(), program.root(), program.split(), &class);
        let element_plan = crate::analysis::element::element_plan(program.nodes(), program.root(), program.split());
        let (element, element_construct, element_collect) = match element_plan {
            Some(plan) => (Some(plan.demand), plan.construct, plan.collect),
            None => (None, None, false),
        };
        // Derived for every program whose reads bound below the whole
        // document: whole-document lowerings attach the tree at the root,
        // and a static forward prefix re-anchors it onto the located node.
        let (prune, pulled_record) = crate::analysis::prune_trees(program.nodes(), program.root(), program.slots());
        let shortcut = commit_shortcut(&program, count, element, element_construct, element_collect);
        let access = commit_access(&program, &shortcut);
        let host = match &shortcut {
            Shortcut::Identity => HostIo::Echo,
            Shortcut::RangeLocate => HostIo::SpanCut,
            _ => HostIo::Run,
        };
        let has_indexed_scans = !program.scans().is_empty() || !program.anti_joins().is_empty();
        let consumes_whole =
            crate::analysis::consumes_whole_document(program.nodes(), program.root(), has_indexed_scans);
        let plan = AccessPlan::pack(&program, &shortcut, access, prune.as_ref(), policy, consumes_whole);
        Self {
            program,
            policy,
            uses_inputs_cursor,
            runtime_index_slot,
            shortcut,
            plan,
            host,
            consumes_whole_document: consumes_whole,
            pulled_record,
        }
    }

    /// The job finish committed. Execute matches this once per record.
    #[must_use]
    pub(crate) fn shortcut(&self) -> &Shortcut {
        &self.shortcut
    }

    /// Packed access kind the codec binds. Tests pin Whole / Exact.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn access(&self) -> Access {
        self.plan.kind
    }

    /// Host I/O this job may ask for. The SDK matches this once; it does not
    /// if-ladder identity then range-locate then residual. Range-locate is
    /// [`HostIo::SpanCut`].
    #[must_use]
    pub const fn host_io(&self) -> HostIo {
        self.host
    }

    /// Whether execute may return [`crate::exec::EngineRun::ReboundWhole`].
    ///
    /// JSON Exact miss cannot run the graph on the located node: that node is
    /// republished as the document root. Reads the packed plan (Exact plus a
    /// count or element hint), not a second demand walk. The host binds Whole
    /// once per drive when the codec access is not already Whole.
    #[must_use]
    pub fn may_rebind_whole(&self) -> bool {
        self.plan.may_rebind_whole()
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
}
