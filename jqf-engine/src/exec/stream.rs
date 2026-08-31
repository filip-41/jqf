//! The public run stream and the typed run entry.
//!
//! One job: hide [`super::StageMachine`] and [`super::GraphMachine`] behind
//! [`EngineRunStream`], and expose [`try_run_program`] / [`EngineRun`] as the
//! crate's poll surface. Production residual seed is [`try_run_program`]
//! (`&Program` plus skip). Sibling modules own evaluation; this file owns
//! the facade.

#[allow(clippy::wildcard_imports)]
use super::*;

/// The two executors, dispatched behind the opaque [`EngineRunStream`].
#[allow(
    clippy::large_enum_variant,
    reason = "the Graph arm carries the machine by value on purpose: it is the common case, \
              constructed and consumed once per input value, and boxing it would add a per-value \
              heap allocation on the hot NDJSON path the record vertical runs"
)]
pub(crate) enum Machine<'program, 'source> {
    /// The fast-path stage executor for a bare-`Stage` residual.
    Stage(StageMachine<'program, 'source>),
    /// The graph executor for a `Choice`/`FlatMap` residual.
    Graph(GraphMachine<'program, 'source>),
    /// Owned values from a document oracle (count, keys, fan-out, …).
    Owned(OwnedStream),
    /// A shortcut that proved a typed failure without seeding a graph.
    Fail(Option<EngineRunError>),
}

/// Polls one owned value per advance. Used by document oracles that already
/// hold every published item.
pub(crate) struct OwnedStream {
    items: alloc::vec::IntoIter<Value>,
    iteration_cap: Option<u64>,
    iterations_done: u64,
}

impl core::fmt::Debug for OwnedStream {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("OwnedStream")
            .field("remaining", &self.items.len())
            .finish_non_exhaustive()
    }
}

/// The iterative executor driving one residual to an ordered result stream.
///
/// A bare-`Stage` residual (the pure path/iteration subset) takes the landed
/// single-slot [`StageMachine`] fast path structurally; a `Choice`/`FlatMap`
/// residual engages the [`GraphMachine`]. Both are hidden behind this one opaque
/// [`poll`] surface, so the SDK adapter drives either without knowing which.
///
/// [`poll`]: EngineRunStream::poll
pub struct EngineRunStream<'program, 'source> {
    machine: Machine<'program, 'source>,
}
impl core::fmt::Debug for EngineRunStream<'_, '_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.machine {
            Machine::Stage(machine) => machine.fmt(formatter),
            Machine::Graph(machine) => machine.fmt(formatter),
            Machine::Owned(owned) => owned.fmt(formatter),
            Machine::Fail(_) => formatter.write_str("Fail"),
        }
    }
}

impl<'program, 'source> EngineRunStream<'program, 'source> {
    /// Drains the fact-write deltas this run recorded, in write order.
    ///
    /// Empty for every program without a `ProgramNode::FactAssign`; the edit
    /// lane drains this once after polling the run to completion.
    #[must_use]
    pub fn take_fact_deltas(&mut self) -> Vec<FactDelta> {
        match &mut self.machine {
            Machine::Stage(_) | Machine::Owned(_) | Machine::Fail(_) => Vec::new(),
            Machine::Graph(machine) => core::mem::take(&mut machine.fact_deltas),
        }
    }

    /// The fact-write overlay recorded so far, in write order.
    ///
    /// Empty for every program without a `ProgramNode::FactAssign`. Query-time
    /// encode peeks this without draining so the edit lane can still take it.
    #[must_use]
    pub fn fact_deltas(&self) -> &[FactDelta] {
        match &self.machine {
            Machine::Stage(_) | Machine::Owned(_) | Machine::Fail(_) => &[],
            Machine::Graph(machine) => machine.fact_deltas.as_slice(),
        }
    }

    /// Seeds an executor for `root` in `nodes` over one input value, applying the
    /// pushdown `prefix_len` at the `entry` stage the split named. A bare-`Stage` root takes the
    /// fast path STRUCTURALLY — its residual steps are sliced and driven by the
    /// [`StageMachine`], engaging no graph dispatch; a `Choice`/`FlatMap`/
    /// constructor root engages the [`GraphMachine`]. A bare literal-start stage
    /// root ignores the input and starts from the owned literal (a per-input
    /// `try_clone`, which is why seeding is fallible).
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when cloning a literal start for this input fails.
    #[allow(
        clippy::too_many_arguments,
        reason = "the seed keeps every arena, boundary, env, and table ownership boundary explicit"
    )]
    pub(crate) fn seed(
        nodes: &'program [ProgramNode],
        root: ProgramNodeId,
        entry: ProgramNodeId,
        prefix_len: usize,
        slots: u32,
        scans: &'program [CorrelatedScan],
        topk: &'program [PartialSort],
        anti: &'program [AntiJoinScan],
        input: EngineResult<'source>,
        runtime_index: Option<(u32, Value)>,
    ) -> Result<Self, CodecError> {
        let mut seeded = Self {
            machine: match &nodes[root.index()] {
            // A literal-start bare stage ignores the input and starts from the
            // owned literal (prefix_len is 0 for a literal entry, so the whole
            // step list is the residual over the literal).
            ProgramNode::Stage {
                start: StageStart::Current,
                steps,
            } => Machine::Stage(StageMachine::seed(&steps[prefix_len..], prefix_len, input)),
            ProgramNode::Stage {
                start: StageStart::Literal(literal),
                steps,
            } => {
                let value = EngineResult::owned(
                    literal
                        .clone(),
                );
                Machine::Stage(StageMachine::seed(&steps[prefix_len..], prefix_len, value))
            }
            // A `Variable`-start root is unreachable (no binder encloses the
            // program root), but it reads the env, so it takes the graph machine
            // rather than the env-less fast path.
            ProgramNode::Stage {
                start: StageStart::Variable(_),
                ..
            }
            | ProgramNode::Choice { .. }
            | ProgramNode::FlatMap { .. }
            | ProgramNode::CollectArray { .. }
            | ProgramNode::CountCollect { .. }
            | ProgramNode::ConstructObject { .. }
            | ProgramNode::Call { .. }
            | ProgramNode::CallDef { .. }
            | ProgramNode::CallFilter { .. }
            | ProgramNode::Binary { .. }
            | ProgramNode::Concat { .. }
            | ProgramNode::Conditional { .. }
            | ProgramNode::Alternative { .. }
            | ProgramNode::Logical { .. }
            | ProgramNode::Try { .. }
            | ProgramNode::ChainBody { .. }
            | ProgramNode::Empty
            | ProgramNode::Bind { .. }
            | ProgramNode::Reduce { .. }
            | ProgramNode::Foreach { .. }
            // A `Counted` root drives its source on the frame stack (and cuts by
            // truncating it), so it takes the graph machine.
            | ProgramNode::Counted { .. }
            // A `Label`/`Break` root needs the barrier stack, so it takes the
            // graph machine — the bare-`Stage` fast path has no frames to unwind.
            | ProgramNode::Label { .. }
            | ProgramNode::Break { .. }
            // A `Modify` root drives its fold on the frame stack.
            | ProgramNode::Modify { .. }
            // A `FactAssign` root drives the same frame stack (the edit lane's
            // fact-write drive; the span authority is the seed document).
            | ProgramNode::FactAssign { .. }
            // An engine root needs the cursor store and the frame stack, exactly
            // like `Modify` (an `EngineBind` seeds a cursor and drives its body;
            // a pull drives the cursor machine; the cursor body is only ever a
            // nested machine's root).
            | ProgramNode::EngineBind { .. }
            | ProgramNode::EnginePull { .. }
            | ProgramNode::EngineGenerator { .. }
            | ProgramNode::EngineRng { .. } => Machine::Graph(GraphMachine::seed(
                nodes, root, entry, prefix_len, slots, scans, topk, anti, input,
            )
            .map_err(|_| CodecError::new(CodecFailureKind::AllocationFailure))?),
        },
        };
        // The SPLIT lane's `$index` preload: when the compile
        // resolved an unbound `$index` to a runtime slot, size the env to the
        // program's slot count and write the item counter into that slot
        // before the first poll. A `$index`-bearing program always takes the
        // graph machine (a `Variable`-start stage is graph by the match
        // above); the stage fast path has no env and cannot reference the
        // slot, so a defensive miss is a no-op there.
        if let Some((slot, value)) = runtime_index
            && let Machine::Graph(machine) = &mut seeded.machine
        {
            machine.env.clear();
            machine
                .env
                .try_reserve(slots as usize)
                .map_err(|_| CodecError::new(CodecFailureKind::AllocationFailure))?;
            let slots = slots as usize;
            machine.env.resize_with(slots, || EngineResult::owned(Value::Null));
            machine.env[slot as usize] = EngineResult::owned(value);
        }
        Ok(seeded)
    }

    /// Seeds from [`Program`]: tables come from the program, and a graph
    /// machine holds that borrow so nested seeds inherit through
    /// [`GraphMachine::seed_nested`]. `topk` is the only table the caller
    /// may override (the recognizer floor passes `&[]`).
    pub(crate) fn from_program(
        program: &'program Program,
        prefix_len: usize,
        input: EngineResult<'source>,
        runtime_index: Option<(u32, Value)>,
        topk: &'program [PartialSort],
    ) -> Result<Self, CodecError> {
        let mut seeded = Self::seed(
            program.nodes(),
            program.root(),
            program.split().entry(),
            prefix_len,
            program.slots(),
            program.scans(),
            topk,
            program.anti_joins(),
            input,
            runtime_index,
        )?;
        if let Machine::Graph(machine) = &mut seeded.machine {
            machine.program = Some(program);
        }
        Ok(seeded)
    }

    /// Whether a graph machine seeded here holds [`Program`]. Stage residuals
    /// stay steps-only.
    #[cfg(test)]
    pub(crate) fn graph_holds_program(&self) -> Option<bool> {
        match &self.machine {
            Machine::Graph(machine) => Some(machine.program.is_some()),
            _ => None,
        }
    }

    /// Seeds the fast-path stage executor directly over a residual step slice.
    /// Used by the executor's own unit tests to exercise the stage machine in
    /// isolation; the run path reaches it through [`EngineRunStream::seed`].
    #[cfg(test)]
    pub(crate) fn seed_stage(steps: &'program [StageStep], base: usize, input: EngineResult<'source>) -> Self {
        Self {
            machine: Machine::Stage(StageMachine::seed(steps, base, input)),
        }
    }

    /// One owned value, identity residual. Compile shortcuts that publish a
    /// single answer (count, keys, has, type, fold).
    pub(crate) fn seed_value(value: Value) -> Self {
        const EMPTY: &[StageStep] = &[];
        Self {
            machine: Machine::Stage(StageMachine::seed(EMPTY, 0, EngineResult::owned(value))),
        }
    }

    /// Ordered owned values from an element fan-out.
    pub(crate) fn seed_owned(values: Vec<Value>) -> Self {
        Self {
            machine: Machine::Owned(OwnedStream {
                items: values.into_iter(),
                iteration_cap: None,
                iterations_done: 0,
            }),
        }
    }

    /// A typed shortcut failure (for example `length` over a boolean).
    pub(crate) fn seed_fail(error: EngineRunError) -> Self {
        Self {
            machine: Machine::Fail(Some(error)),
        }
    }

    /// Advances the stream by at most one ordered item, charging one work
    /// transition per advance and honoring cooperative pauses.
    ///
    /// # Errors
    ///
    /// Returns [`EngineRunError`]: a typed path/iterate mismatch discovered
    /// mid-iteration, or a machine failure (control, ledger, internal contract).
    pub fn poll(&mut self, resources: &mut ResourceContext<'_>) -> Result<RunPoll<'source>, EngineRunError> {
        match &mut self.machine {
            Machine::Stage(machine) => machine.poll(resources),
            Machine::Graph(machine) => machine.poll(resources),
            Machine::Owned(owned) => owned.poll(resources),
            Machine::Fail(error) => match error.take() {
                Some(error) => Err(error),
                None => Ok(RunPoll::Complete),
            },
        }
    }

    /// Caps this run's cumulative frame-task transitions (`--max-iterations`).
    ///
    /// The counter is enforced beside every cooperative work admission in both
    /// machines; crossing the cap raises the machine resource refusal
    /// ([`iteration_ceiling_error`]) — not catch-eligible, exit class 5.
    /// `None` (the default from [`Self::seed`]) is unlimited and costs one
    /// `None` test per transition.
    #[must_use]
    pub fn with_iteration_cap(mut self, cap: Option<u64>) -> Self {
        match &mut self.machine {
            Machine::Stage(machine) => machine.iteration_cap = cap,
            Machine::Graph(machine) => machine.iteration_cap = cap,
            Machine::Owned(owned) => owned.iteration_cap = cap,
            Machine::Fail(_) => {}
        }
        self
    }
}

impl OwnedStream {
    fn poll(&mut self, resources: &mut ResourceContext<'_>) -> Result<RunPoll<'static>, EngineRunError> {
        if self.items.as_slice().is_empty() {
            return Ok(RunPoll::Complete);
        }
        match resources
            .admit_work_transition()
            .map_err(|error| EngineRunError::from(EngineError::Control(error)))?
        {
            WorkAdmission::Pending => return Ok(RunPoll::Pending),
            WorkAdmission::Granted(_) => {}
        }
        admit_iteration(&mut self.iterations_done, self.iteration_cap)?;
        match self.items.next() {
            Some(value) => Ok(RunPoll::Item(EngineResult::owned(value))),
            None => Ok(RunPoll::Complete),
        }
    }
}

/// Which input the residual stream runs over — the interpretation tag the SDK
/// maps onto its publication disposition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunInput {
    /// A resolved located value: the ordinary emitted disposition.
    Resolved,
    /// The owned `null` a pushed-down missing path or mismatch-on-null produced.
    /// The residual still runs over it (`.a` forwards one null; `.a[]` iterates
    /// null and errors; `.a[]?` suppresses) — so a `Missing`-tagged run can
    /// publish one null, zero items, or abort.
    Missing,
}

/// The engine-typed result of launching a compiled program on one codec input.
///
/// The engine owns the parity interpretation of the pushed-down access
/// outcome. Everything flows through the residual: [`EngineRun::Stream`] carries
/// the executor over an input item (a resolved located value, or the owned null a
/// missing/mismatch-on-null produced, tagged by [`RunInput`]). The two
/// pre-residual arms fire only when the pushed-down step itself failed, so no
/// input exists to iterate: [`EngineRun::Suppressed`] (a flagged-step mismatch)
/// and [`EngineRun::Pushdown`] (an unflagged one). [`EngineRun::ReboundWhole`]
/// is Exact count or element miss on a document whose root *is* the selection
/// (JSON Exact): skip is not `demand.path`, so the host decodes Whole and
/// runs the graph.
#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "the Stream arm carries the executor by value on purpose: it is the common case, \
              constructed and consumed once per input value, and boxing it would add a per-value \
              heap allocation on the hot NDJSON path the vertical runs"
)]
pub enum EngineRun<'program, 'source> {
    /// The residual executor over an input item, tagged by input provenance.
    Stream {
        /// The ordered executor stream.
        stream: EngineRunStream<'program, 'source>,
        /// Whether the input is a resolved value or a pushed-down null.
        input: RunInput,
    },
    /// A non-null mismatch at a pushed-down step whose per-component `?` flag is
    /// set: pre-residual suppression (zero items, nothing is printed, exit 0).
    Suppressed,
    /// A concrete non-null pushed-down mismatch with no suppressing flag at the
    /// failing step: a typed abort, in the SAME typed vocabulary a mid-residual
    /// failure uses, already carrying the rendered accessor. Sharing the
    /// vocabulary is what lets a host classify and render a pushed-down mismatch
    /// and a mid-fan-out one through one path instead of two.
    ///
    /// It is always the index class: the codec resolves only a static
    /// `Key`/`Index` prefix, so an iterate mismatch reaches the residual instead.
    Pushdown(EngineRunError),
    /// Exact count or element miss on a Located document whose root *is* the
    /// selection. JSON Exact republishes PATH as the document root, so
    /// `node == root` cannot tell Exact from Whole. Residual skip is
    /// `split.prefix_len`, not `demand.path` — running Whole on this outcome
    /// walks PATH twice. The host rebinds Whole-document access and calls
    /// [`crate::CompiledProgram::try_run_whole_value`].
    ReboundWhole,
}

impl EngineRun<'_, '_> {
    /// Caps this run's cumulative frame-task transitions (`--max-iterations`).
    /// A builder over [`EngineRunStream::with_iteration_cap`]; the
    /// non-stream arms (a suppressed or pushed-down mismatch — no run exists)
    /// ignore the cap and return unchanged.
    #[must_use]
    pub fn with_iteration_cap(self, cap: Option<u64>) -> Self {
        match self {
            Self::Stream { stream, input } => Self::Stream {
                stream: stream.with_iteration_cap(cap),
                input,
            },
            other => other,
        }
    }
}

/// Interprets one codec input outcome into an engine-typed run under the
/// program's per-component optional (`?`) flags and pushdown boundary.
///
/// Production residual seed is [`try_run_program`]: `&Program` plus skip.
/// This slice form is the test/floor unpack (raw arena, optional top-k
/// override). The reading, in this exact precedence:
///
/// 1. a resolved value → [`EngineRun::Stream`] over the residual, tagged
///    `Resolved` (identity residual forwards it; a graph residual runs it);
/// 2. a missing path, or a mismatch whose actual value is null → an owned `null`
///    input item, [`EngineRun::Stream`] tagged `Missing` (`{} | .a` → null,
///    `{} | .a[]` → iterate-null error, `{} | .a[]?` → empty). The null check
///    runs before the flag check, matching `{"a":null} | .a.b?` → null;
/// 3. a non-null mismatch at a flagged pushed-down step → [`EngineRun::Suppressed`];
/// 4. any other non-null mismatch → the typed [`EngineRun::Pushdown`] abort.
///
/// A pushed-down `Missing`/`TypeMismatch` arises only for a scoped prefix
/// (`prefix_len > 0`, so the entry is a real `Stage`); a whole-document route
/// yields a resolved root and never reaches arms 2-4 for a comma-rooted graph.
///
/// # Errors
///
/// Returns [`CodecError`] only when seeding the executor against the request
/// ledger fails; the typed error arms are `Ok` variants.
fn interpret_outcome<'program, 'source>(
    outcome: CodecInputOutcome<'source>,
    entry_steps: &'program [StageStep],
    seed: impl FnOnce(EngineResult<'source>) -> Result<EngineRunStream<'program, 'source>, CodecError>,
) -> Result<EngineRun<'program, 'source>, CodecError> {
    Ok(match outcome {
        CodecInputOutcome::Result(result) => EngineRun::Stream {
            stream: seed(result)?,
            input: RunInput::Resolved,
        },
        CodecInputOutcome::Missing { .. }
        | CodecInputOutcome::TypeMismatch {
            actual_type: ValueKind::Null,
            ..
        } => EngineRun::Stream {
            stream: seed(EngineResult::owned(Value::Null))?,
            input: RunInput::Missing,
        },
        CodecInputOutcome::TypeMismatch {
            step_index,
            actual_type,
            hint,
            ..
        } => {
            // Exact-step law: only a `?` on the failing pushed-down step itself
            // suppresses. `step_index` indexes the entry stage's pushed-down
            // prefix, which the codec resolved.
            let step = entry_steps.get(step_index);
            if step.is_some_and(StageStep::is_optional) {
                EngineRun::Suppressed
            } else {
                EngineRun::Pushdown(pushdown_mismatch(step, step_index, actual_type, hint)?)
            }
        }
    })
}

/// Residual seed from [`Program`] plus skip. Nested machines inherit tables
/// through [`GraphMachine::seed_nested`]. `prefix_len == 0` is Whole.
///
/// # Errors
///
/// Returns [`CodecError`] only when seeding the executor against the request
/// ledger fails; the typed error arms are `Ok` variants.
pub(crate) fn try_run_program<'program, 'source>(
    program: &'program Program,
    prefix_len: usize,
    outcome: CodecInputOutcome<'source>,
    runtime_index: Option<(u32, Value)>,
) -> Result<EngineRun<'program, 'source>, CodecError> {
    interpret_outcome(outcome, program.entry_stage_steps(), |input| {
        EngineRunStream::from_program(program, prefix_len, input, runtime_index, program.topk_sorts())
    })
}

/// Residual seed from [`Program`] with an explicit partial-sort table.
///
/// The recognizer differential's forced floor passes `&[]`. Production
/// [`try_run_program`] uses [`Program::topk_sorts`].
#[cfg(test)]
pub(crate) fn try_run_with_table<'program, 'source>(
    program: &'program Program,
    outcome: CodecInputOutcome<'source>,
    topk: &'program [PartialSort],
) -> Result<EngineRun<'program, 'source>, CodecError> {
    interpret_outcome(outcome, program.entry_stage_steps(), |input| {
        EngineRunStream::from_program(program, program.split().prefix_len(), input, None, topk)
    })
}

/// Builds the typed error for one pushed-down mismatch, rendering the accessor
/// text out of the failing step exactly as the residual would.
///
/// It is always the INDEX class. The codec resolves only a STATIC prefix — every
/// requirement lowering rejects `.[]`, `..`, `.[$x]`, and slices — so a
/// codec-reported mismatch always lands on a `Key` or an `Index` step, and the
/// index message needs exactly what those carry: the container's kind and the
/// accessor. A `.[]` over a non-iterable therefore always surfaces mid-residual
/// (where the operand exists to interpolate), never here.
pub(crate) fn pushdown_mismatch(
    step: Option<&StageStep>,
    step_index: usize,
    actual_type: ValueKind,
    hint: Option<alloc::string::String>,
) -> Result<EngineRunError, CodecError> {
    let key = match step.map(StageStep::access) {
        Some(StepAccess::Key(name)) => message::render_object_key(name),
        Some(StepAccess::Index(index)) => message::render_array_key(*index),
        // Every requirement lowering REJECTS a non-static step, so the codec can
        // only report a mismatch inside a `Key`/`Index` prefix. Reaching here
        // means a lowering handed the codec a step it promised never to.
        _ => {
            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "codec pushdown mismatch outside the static prefix",
            }));
        }
    };
    Ok(EngineRunError::TypeMismatch {
        step_index,
        actual_type,
        key: key.map_err(|_| CodecError::new(CodecFailureKind::AllocationFailure))?,
        hint,
    })
}
