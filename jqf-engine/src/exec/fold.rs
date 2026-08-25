//! Bind, countdown, and fold consumers on the graph machine.
//!
//! One job: drive `as $x`, `limit`/`nth`/`skip`, and `reduce`/`foreach`.
//! A countdown cut parks frames pushed during [`super::GraphMachine::route`]
//! and reconciles the captured continuation against the shorter stack.

#[allow(clippy::wildcard_imports)]
use super::*;

impl<'program, 'source> GraphMachine<'program, 'source> {
    /// Evaluates `SOURCE as $x | BODY` over `input`: retain the ORIGINAL input in
    /// a `BindBody` consumer frame (the body's dot — a binding does not change
    /// dot), then drive the source over a re-borrow of it.
    pub(crate) fn eval_bind(
        &mut self,
        source: ProgramNodeId,
        slot: VarSlot,
        body: ProgramNodeId,
        frame: bool,
        input: EngineResult<'source>,
        cont: usize,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        // A PATTERN-FRAME extraction binder's matcher steps are TRACKED (the
        // matchers index with constant keys), so only the plain as-var
        // binder freezes its source.
        if !frame {
            self.path_enter_frozen();
        }
        let source_input = input.try_clone().map_err(EngineRunError::Codec)?;
        self.push_frame(GraphFrame::BindBody {
            slot,
            body,
            input,
            out_cont: cont,
            collected: Vec::new(),
            cursor: 0,
            frame,
        });
        self.pending = Some(GraphTask::Eval {
            node: source,
            input: source_input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Consumes one source output for a `Bind`: write it into the binder's slot,
    /// then evaluate the body over the retained original input. The body runs to
    /// completion (depth-first) before the source resumes, so bodies run once per
    /// source output IN SOURCE ORDER.
    ///
    /// The input is re-BORROWED while the source can still emit and MOVED once it
    /// cannot ([`Self::producer_spent_above`]). The move is what keeps a binder out
    /// of an in-place accumulator's way: `$k as $v | .[$k] = $v` inside a fold
    /// hands `set_path` the fold state, and a second live handle turns each
    /// write's copy-on-write detach into a whole-container rebuild (O(n) per
    /// write, O(n²) over the fold); the retained handle is dead the moment the
    /// source cannot produce again, so the last body run has nothing to retain
    /// it for.
    pub(crate) fn consume_bind_body(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        // A register-active multi-output PLAIN source COLLECTS its outputs
        // (the frozen scope spans them all); the bodies replay once the
        // source completes. Single-output and pattern-frame binders run
        // inline (a frame binder's body runs inside the matcher's scope).
        let frame = match self.frames.as_slice().get(index) {
            Some(GraphFrame::BindBody { frame, .. }) => *frame,
            _ => return Err(internal_contract("bind frame kind changed under capture")),
        };
        if self.path.is_some() && !frame {
            // Share the path body's memoized root: `. as $x` must bind the
            // same allocation the register addresses (Law 1). A fresh
            // capture of a located `.` would be a twin and fail the check.
            let owned = self.materialize_subject_memoized(value, resources)?;
            let GraphFrame::BindBody { collected, .. } = &mut self.frames.as_mut_slice()[index] else {
                return Err(internal_contract("bind frame kind changed under capture"));
            };
            collected
                .try_reserve(1)
                .map_err(|_| EngineRunError::allocation_failure())?;
            collected.push(owned);
            return Ok(None);
        }
        let spent = self.producer_spent_above(index);
        let (slot, body, input, out_cont) = match self.frames.as_mut_slice().get_mut(index) {
            Some(GraphFrame::BindBody {
                slot,
                body,
                input,
                out_cont,
                ..
            }) => (*slot, *body, take_or_borrow(input, spent)?, *out_cont),
            _ => return Err(internal_contract("bind frame kind changed under capture")),
        };
        // The source's frozen scope ends: the body runs with the register
        // as the bind opened it.
        self.path_exit_frozen();
        self.write_slot(slot, value)?;
        self.pending = Some(GraphTask::Eval {
            node: body,
            input,
            cont: out_cont,
        });
        Ok(None)
    }

    /// Evaluates `~CONSTRUCTOR as ~x | BODY`: seeds the cursor
    /// machine for the constructor over a CLONE of the binding-site dot with an
    /// env snapshot of every live slot, stores it in this machine's cursor
    /// store, pushes the RAII release barrier, and runs the body once over the
    /// ORIGINAL input (a pull sees the parent's dot unchanged). A re-evaluation
    /// of this node replaces the slot's cursor, dropping the old one.
    pub(crate) fn eval_engine_bind(
        &mut self,
        source: ProgramNodeId,
        slot: EngineSlot,
        body: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        // Grow the cursor store to cover the slot on demand; a program with no
        // engine bindings never allocates it at all.
        let index = slot.index();
        if self.cursors.len() <= index {
            self.cursors
                .try_reserve(index + 1 - self.cursors.len())
                .map_err(|_| EngineRunError::allocation_failure())?;
            self.cursors.resize_with(index + 1, || None);
        }
        let seed_input = input.try_clone().map_err(EngineRunError::Codec)?;
        let mut cursor = GraphMachine::seed(
            self.nodes, source, source, 0, self.slots, self.scans, self.topk, self.anti, seed_input,
        )?;
        cursor.inherit_fact_deltas(self);
        // The fold drive runs inside the parent's cooperative admissions, but
        // the CURSOR is its own machine with its own counter, so it inherits
        // the run's iteration ceiling directly.
        cursor.iteration_cap = self.iteration_cap;
        // The ENV SNAPSHOT: the constructor's graphs (and the cursor's whole
        // drive) see the bindings in lexical scope at the bind site, frozen at
        // bind time — a located slot re-borrows (a refcount bump), an owned one
        // clones (the O(slots) env snapshot). Every slot the
        // cursor's graphs can read was written by an outer binder before this
        // bind evaluated, so the copied env covers them all.
        for (env_index, value) in self.env.as_slice().iter().enumerate() {
            cursor.write_slot(
                u32::try_from(env_index).map_err(|_| internal_contract("env slot index overflow"))?,
                value.try_clone().map_err(EngineRunError::Codec)?,
            )?;
        }
        self.cursors[index] = Some(Box::new(cursor));
        // The body's outputs route PAST the barrier to the binding's own
        // continuation; the barrier pops when the body's single evaluation
        // completes, releasing the cursor (RAII — scope exit closes).
        self.push_frame(GraphFrame::EngineBindBody { slot, out_cont: cont });
        self.pending = Some(GraphTask::Eval {
            node: body,
            input,
            cont,
        });
        Ok(None)
    }

    /// Evaluates an [`ProgramNode::EngineGenerator`] — the CURSOR machine's
    /// root. Pushes the phase drive and seeds `init` over the machine's input
    /// (the binding-site dot). Only the nested cursor machine ever evaluates
    /// this node; the enclosing machine rejects it at dispatch.
    pub(crate) fn eval_engine_generator(
        &mut self,
        node: ProgramNodeId,
        input: EngineResult<'source>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let ProgramNode::EngineGenerator { init, .. } = &self.nodes[node.index()] else {
            return Err(internal_contract("engine generator drive lost its generator node"));
        };
        self.push_frame(GraphFrame::EngineGen {
            node,
            phase: EngineGenPhase::Init,
            state: None,
            emitted: false,
            out_cont: 0,
        });
        self.pending = Some(GraphTask::Eval {
            node: *init,
            input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Consumes one output of the cursor's current phase graph — the
    /// `~generator` cardinality law, one state and one emission per pull:
    /// an `init`/`update` output becomes the state (a second output raises the
    /// typed cardinality error), an `extract` output is the pull's emitted
    /// value, routed through the cursor machine's own output ceiling.
    pub(crate) fn consume_engine_gen(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let (phase, out_cont) = match self.frames.as_slice().get(index) {
            Some(GraphFrame::EngineGen { phase, out_cont, .. }) => (*phase, *out_cont),
            _ => {
                return Err(internal_contract("engine generator frame kind changed under capture"));
            }
        };
        let already = match self.frames.as_mut_slice().get_mut(index) {
            // The Init/Update phases track the single state output; the Extract
            // phase tracks its single emission with the dedicated flag.
            Some(GraphFrame::EngineGen {
                phase: EngineGenPhase::Init | EngineGenPhase::Update,
                state,
                ..
            }) => state.is_some(),
            Some(GraphFrame::EngineGen {
                phase: EngineGenPhase::Extract,
                emitted,
                ..
            }) => *emitted,
            _ => {
                return Err(internal_contract("engine generator frame kind changed under capture"));
            }
        };
        if already {
            self.raise_cont = out_cont;
            return Err(EngineRunError::EngineCardinality {
                constructor: "generator",
                phase: match phase {
                    EngineGenPhase::Init => "init",
                    EngineGenPhase::Update => "update",
                    EngineGenPhase::Extract => "extract",
                },
            });
        }
        match phase {
            // The state is held (with its own ledger residency), exactly like a
            // fold's state cell.
            EngineGenPhase::Init | EngineGenPhase::Update => {
                let cell = self.hold_state(value, resources)?;
                match self.frames.as_mut_slice().get_mut(index) {
                    Some(GraphFrame::EngineGen { state, .. }) => *state = Some(cell),
                    _ => {
                        return Err(internal_contract("engine generator frame kind changed under capture"));
                    }
                }
                Ok(None)
            }
            // The extract output IS the emission: mark it emitted and route it
            // through the cursor machine's own ceiling (no frames above the
            // generator at the machine's root), so the machine's next poll
            // yields it as an item.
            EngineGenPhase::Extract => {
                match self.frames.as_mut_slice().get_mut(index) {
                    Some(GraphFrame::EngineGen { emitted, .. }) => *emitted = true,
                    _ => {
                        return Err(internal_contract("engine generator frame kind changed under capture"));
                    }
                }
                self.route(value, out_cont, resources)
            }
        }
    }

    /// Evaluates an [`ProgramNode::EngineRng`] — the `~rng($seed)` cursor
    /// machine's root. Pushes the `Seed` phase and drives the seed
    /// graph over the machine's input (the binding-site dot). Only the nested
    /// cursor machine ever evaluates this node; the enclosing machine rejects
    /// it at dispatch.
    pub(crate) fn eval_engine_rng(
        &mut self,
        node: ProgramNodeId,
        input: EngineResult<'source>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let ProgramNode::EngineRng { seed } = &self.nodes[node.index()] else {
            return Err(internal_contract("engine rng drive lost its rng node"));
        };
        self.push_frame(GraphFrame::EngineRng {
            phase: EngineRngPhase::Seed,
            rng: None,
            out_cont: 0,
        });
        self.pending = Some(GraphTask::Eval {
            node: *seed,
            input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Consumes the `~rng` seed's single output — the one state the `Seed`
    /// phase admits, exactly like the generator's init: a second output raises
    /// the typed cardinality error. The output must be an exact integer; the
    /// seeded generator is armed and the drive switches to `Emit`.
    pub(crate) fn consume_engine_rng(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let (out_cont, already) = match self.frames.as_slice().get(index) {
            Some(GraphFrame::EngineRng { rng, out_cont, .. }) => (*out_cont, rng.is_some()),
            _ => {
                return Err(internal_contract("engine rng frame kind changed under capture"));
            }
        };
        if already {
            self.raise_cont = out_cont;
            return Err(EngineRunError::EngineCardinality {
                constructor: "rng",
                phase: "seed",
            });
        }
        let seed = self.materialize_uncharged(value, resources)?;
        let seed = match seed {
            Value::Number(number) => number.to_i64(),
            _ => None,
        }
        .ok_or_else(|| {
            self.raise_cont = out_cont;
            semantics_path::raise("~rng seed must be an integer", resources)
        })?;
        let rng = Prng::from_seed(u64::from_ne_bytes(seed.to_ne_bytes()));
        match self.frames.as_mut_slice().get_mut(index) {
            Some(GraphFrame::EngineRng { rng: armed, phase, .. }) => {
                *armed = Some(rng);
                *phase = EngineRngPhase::Emit;
            }
            _ => {
                return Err(internal_contract("engine rng frame kind changed under capture"));
            }
        }
        Ok(None)
    }

    /// The release duties IDENTICAL on every pop path of a frame: the engine
    /// binding's cursor release and the suppression-region exit the two
    /// barrier kinds entered at push. Shared by [`Self::pop_frame_releasing`]
    /// (the raise walk) and [`Self::trim_spent_frames`] so neither can drift
    /// from the other when a frame kind's duties change. The frozen-position
    /// frames are deliberately NOT here: a raise-abandoned frame ABANDONS its
    /// scope while a normally-spent one EXITS it — different register laws.
    fn release_frame_duties(&mut self, frame: &GraphFrame<'program, 'source>, resources: &ResourceContext<'_>) {
        if let GraphFrame::EngineBindBody { slot, .. } = frame {
            self.release_engine_cursor(*slot);
        }
        if matches!(frame, GraphFrame::Try { .. } | GraphFrame::AlternativeLeft { .. }) {
            resources.exit_mismatch_suppression();
        }
    }

    /// Pops the top frame, releasing the suppression region it bracketed when
    /// it is a `Try` or `AlternativeLeft`. The two suppression
    /// frames enter a depth at push and must exit it at EVERY pop — the raise
    /// walk, the cuts, and the spent trim included — so the sites that pop
    /// them go through this helper and stay in balance with the live frames.
    pub(crate) fn pop_frame_releasing(
        &mut self,
        resources: &ResourceContext<'_>,
    ) -> Option<GraphFrame<'program, 'source>> {
        let mut frame = self.frames.pop();
        // The RAII release rides EVERY pop path of the binding barrier — the
        // raise walk, the countdown cuts, and this helper's own callers.
        if let Some(popped) = &frame {
            self.release_frame_duties(popped, resources);
        }
        if let Some(GraphFrame::CallFilter {
            saved_env,
            saved_filters,
        }) = &mut frame
        {
            self.env = core::mem::take(saved_env);
            self.filter_env = core::mem::take(saved_filters);
        }
        // A `PathStep` marker popped here (the raise walk) restores the
        // register; a `PathPublish` popped here (the drive abandoned by a
        // raise that escaped it) hands the displaced outer register back.
        if let Some(GraphFrame::PathStep { steps_len, at, at_node }) = &frame
            && let Some(register) = self.path.as_mut()
        {
            register.restore_navigation(*steps_len, at.clone(), *at_node);
        }
        // A frozen-position frame abandoned by the raise walk (or a cut)
        // leaves its frozen scope open: release it.
        if let Some(frame) = &frame
            && path_register::frozen_abandon_frame(frame)
        {
            self.path_abandon_frozen();
        }
        if let Some(GraphFrame::PathPublish { .. }) = &frame {
            // The register the drive installed is restored — `None` when the
            // drive displaced none (the move is required: not `Clone`).
            self.path = match frame.take() {
                Some(GraphFrame::PathPublish { saved, .. }) => saved,
                _ => unreachable!("path publish frame matched by kind"),
            };
        }
        frame
    }

    /// Releases the cursor at `slot` (the RAII law): the binding's
    /// scope ended, so the suspended machine — and every ledger residency it
    /// holds — is dropped. Idempotent; a slot may already be `None` (a nested
    /// evaluator that never bound it).
    pub(crate) fn release_engine_cursor(&mut self, slot: EngineSlot) {
        if let Some(cell) = self.cursors.get_mut(slot.index()) {
            *cell = None;
        }
    }

    /// Reports that the frame at `index` can never receive another output from
    /// the generator it consumes, so a value it retains for the NEXT consumption
    /// may be MOVED out rather than re-borrowed.
    ///
    /// This is an OBSERVATION of the machine's own state, not a prediction about
    /// the generator expression. The machine's entire resumable state is `pending`
    /// plus `frames`; a consumer runs with `pending` already taken, and a consumer
    /// frame launches its generator with ITSELF as the floor, so every frame the
    /// generator can resume through sits ABOVE `index`. No such frame means
    /// nothing is left to resume.
    ///
    /// [`GraphFrame::FlatMapBody`] is the one exception the scan tolerates,
    /// because it is a pure continuation: it holds no value and no cursor, and it
    /// produces only when its OWN upstream routes to it — which needs a live frame
    /// above IT. A stack of nothing but those is therefore a spent generator, and
    /// tolerating them is what lets a piped source (`($x | f) as $k`, the shape
    /// `INDEX` and every keyed fold lower to) take the move.
    ///
    /// Any other frame kind DECLINES, including one that happens to be exhausted:
    /// a needless re-borrow costs a refcount bump, while a wrong move would hand
    /// the next run a `null` dot.
    pub(crate) fn producer_spent_above(&self, index: usize) -> bool {
        self.frames.as_slice().get(index + 1..).is_some_and(|above| {
            above
                .iter()
                .all(|frame| matches!(frame, GraphFrame::FlatMapBody { .. }))
        })
    }

    /// Pops every SPENT frame down to `floor`, and reports whether it got there.
    ///
    /// This is [`Self::producer_spent_above`]'s claim made ACTIONABLE, over the
    /// much larger set of frames the observation actually covers. A frame is
    /// spent when reaching the top of the stack is the whole of what it has left
    /// to do — [`Self::advance_frame`] pops it, sets no task, and publishes
    /// nothing — so a contiguous run of them below the top produces nothing, and
    /// popping the run now is only doing that work early. The run is popped
    /// TOP-DOWN, one frame at a time, so every frame is at the top when it is
    /// popped and the "top means spent" reading is exact for each.
    ///
    /// Popping early is not merely equivalent for the two BARRIER kinds, it is a
    /// correction. A `Try` that outlived its body used to sit under the frames
    /// the NEXT round pushed, and the enclosure test (`out_cont > raise_cont`)
    /// admits a barrier whose out-continuation is below the failure's routing
    /// position — so a spent round's `try` could catch a later round's error.
    /// `[while(if . == 3 then error("boom") else . < 5 end; try (.+1) catch "c")]`
    /// is the shape: the floor raises `boom`, and so does this machine once the
    /// barrier is gone at the moment its body is.
    ///
    /// The caller must have taken `pending` — the scan reads the frame stack as
    /// the machine's whole resumable state, and a pending task is the one thing
    /// that could route into the run from outside it.
    ///
    /// Frames that DO something on the way out are not in the set and stop the
    /// trim where they stand: a producer with a live cursor, an accumulator that
    /// publishes its answer (`CombinationsWidth`, `Collect`, `ReduceSource`), a
    /// `Choice` with a right member still to fire, an `ObjectValue` that must
    /// truncate its destination. Stopping is always safe; it only costs the
    /// caller its optimization.
    pub(crate) fn trim_spent_frames(&mut self, floor: usize, resources: &ResourceContext<'_>) -> bool {
        while self.frames.len() > floor {
            let Some(frame) = self.frames.as_slice().last() else {
                break;
            };
            if !Self::pops_spent(frame) {
                break;
            }
            // A spent frozen-position frame EXITS its scope here — the
            // normal-pop law — not the raise walk's abandon.
            if path_register::frozen_pop_frame(frame) {
                self.path_exit_frozen();
            }
            let popped = self.frames.pop();
            debug_assert!(popped.is_some(), "len checked above");
            // The shared release duties (binding cursor, suppression depth)
            // ride this pop exactly as they ride every other.
            if let Some(popped) = &popped {
                self.release_frame_duties(popped, resources);
            }
        }
        self.frames.len() == floor
    }

    /// Whether [`Self::advance_frame`] would pop this frame with no task, no
    /// output, and no other effect — the one row-for-row companion to that
    /// function's silent-pop arms.
    pub(crate) fn pops_spent(frame: &GraphFrame<'program, 'source>) -> bool {
        match frame {
            GraphFrame::FlatMapBody { .. }
            | GraphFrame::ObjectDest { .. }
            | GraphFrame::ObjectKey { .. }
            | GraphFrame::SelectBody { .. }
            | GraphFrame::BinaryRight { .. }
            | GraphFrame::BinaryLeft { .. }
            | GraphFrame::ConcatPart { .. }
            | GraphFrame::MathArgs { .. }
            | GraphFrame::MathCompute { .. }
            | GraphFrame::ConditionalBody { .. }
            | GraphFrame::LogicalLeft { .. }
            | GraphFrame::LogicalRight { .. }
            | GraphFrame::Try { .. }
            | GraphFrame::Label { .. }
            | GraphFrame::ErrorArg { .. }
            | GraphFrame::ArgumentAnswer { .. }
            | GraphFrame::ReduceInit { .. }
            | GraphFrame::ForeachInit { .. }
            | GraphFrame::ForeachSource { .. }
            | GraphFrame::ReduceUpdate { .. }
            | GraphFrame::ForeachUpdate { .. }
            // An `EngineBindBody` under the top means the binding body's
            // evaluation is complete — the spent-arm pops it the same way, and
            // the trim's own pop releases the cursor (see the trim loop).
            | GraphFrame::EngineBindBody { .. }
            // A `Counted` under the top means its source ran out before its
            // count did, which is the same nothing-left-to-do that puts it in
            // `advance_frame`'s silent-pop arm. Its OTHER exit — the cut — pops
            // the frame itself and everything above it, so a cut frame is never
            // here to be seen.
            | GraphFrame::Counted { .. } => true,
            // The generator family's consumer states pop the same way, with the
            // one accumulator excepted: `CombinationsWidth` PUBLISHES on the way
            // out, so it is a producer for this purpose.
            GraphFrame::Generate { state } => {
                state.consumes() && !matches!(state, GenerateState::CombinationsWidth { .. })
            }
            // A BindBody with collected outputs is NOT spent: it replays them
            // as bodies once the source completes.
            GraphFrame::BindBody { collected, .. } => collected.is_empty(),
            _ => false,
        }
    }

    /// Evaluates a [`ProgramNode::Counted`] over `input`: read the already-bound
    /// count, classify it once, and drive the source into a countdown frame.
    ///
    /// The count is read HERE and never again — the enclosing binder wrote the
    /// slot before this node was reached, and nothing between the two can
    /// rewrite it (the slot is unique to the binder occurrence) — so the whole
    /// classification is a per-drive cost, not a per-item one.
    #[allow(
        clippy::similar_names,
        reason = "the countdown's kind/cont locals are the Counted node's own field names"
    )]
    pub(crate) fn eval_counted(
        &mut self,
        source: ProgramNodeId,
        slot: VarSlot,
        kind: CountedKind,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let bound = match self.env.as_slice().get(slot as usize) {
            Some(result) => result.try_clone().map_err(EngineRunError::Codec)?,
            None => {
                return Err(internal_contract(
                    "a counted stream read an env slot outside the sized env",
                ));
            }
        };
        let value = self.materialize_capture(bound, resources)?;
        let state = classify_count(kind, value);
        self.push_frame(GraphFrame::Counted {
            state,
            kind,
            out_cont: cont,
        });
        self.pending = Some(GraphTask::Eval {
            node: source,
            input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Consumes one source item for a countdown: pay a unit of the count, then
    /// answer per [`CountedKind`].
    ///
    /// The CUT (`limit`'s spent count, `nth`'s hit) hands the item to its
    /// consumer first — the publish check still sees the live `PathStep`
    /// frames — then drops this frame and the leftover source. Frames the
    /// consumer pushed during that hand-off are parked across the drop so no
    /// consumer is left pointing past the stack. See [`GraphMachine::route`]
    /// for the continuation-depth reconciliation that makes the same cut safe
    /// in every position.
    pub(crate) fn consume_counted(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let (kind, out_cont) = match self.frames.as_slice().get(index) {
            Some(GraphFrame::Counted { kind, out_cont, .. }) => (*kind, *out_cont),
            _ => {
                return Err(internal_contract("counted frame kind changed under capture"));
            }
        };
        let paid = match self.frames.as_mut_slice().get_mut(index) {
            Some(GraphFrame::Counted { state, .. }) => counted_verdict(kind, state, resources),
            _ => {
                return Err(internal_contract("counted frame kind changed under capture"));
            }
        };
        let verdict = match paid {
            Ok(verdict) => verdict,
            Err(error) => {
                // The countdown's `. - 1` raises where the `Binary` it replaces
                // raised: at the node's own routing position, which is what
                // decides whose `try` catches it.
                self.raise_cont = out_cont;
                return Err(error);
            }
        };
        match verdict {
            CountedVerdict::Withhold => Ok(None),
            CountedVerdict::Publish => self.route(value, out_cont, resources),
            CountedVerdict::PublishAndCut => {
                // Route FIRST (the publish check reads the PathStep frames),
                // then drop this countdown and its leftover source without
                // taking the frames that route just pushed.
                let source_end = self.frames.len();
                let routed = self.route(value, out_cont, resources)?;
                self.cut_counted_source(index, source_end, resources);
                Ok(routed)
            }
        }
    }

    /// Drops the countdown at `index` and every leftover source frame that
    /// lived above it at `source_end`, keeping frames pushed after `source_end`.
    ///
    /// Those kept frames are a consumer that captured a continuation during
    /// [`Self::route`]; after the drop that continuation is reconciled against
    /// the shorter stack so the next advance still addresses a live frame.
    pub(crate) fn cut_counted_source(&mut self, index: usize, source_end: usize, resources: &ResourceContext<'_>) {
        let parked = (self.frames.len() > source_end).then(|| self.frames.split_off(source_end));
        while self.frames.len() > index {
            self.pop_frame_releasing(resources);
        }
        if let Some(parked) = parked {
            self.frames.extend(parked);
        }
        self.reconcile_pending_cont();
    }

    /// A cut may leave a pending task's `cont` past `frames.len()`. Clamp it
    /// to the live stack — the same reconciliation [`Self::route`] applies
    /// when that task later emits.
    pub(crate) fn reconcile_pending_cont(&mut self) {
        let ceiling = self.frames.len();
        match &mut self.pending {
            Some(GraphTask::Eval { cont, .. } | GraphTask::Descend { cont, .. } | GraphTask::Route { cont, .. }) => {
                *cont = (*cont).min(ceiling);
            }
            None => {}
        }
    }

    /// Evaluates a `reduce`/`foreach` over `input`: retain the ORIGINAL input in
    /// the loop's INIT consumer frame, then drive `init` over a re-borrow of it
    /// (the init sees the outer dot, and `$x` is not yet in scope).
    #[allow(
        clippy::too_many_arguments,
        reason = "the two loop families share one drive; splitting the parameter list into a \
                  struct would add a second vocabulary for the node's own fields"
    )]
    pub(crate) fn eval_loop(
        &mut self,
        source: ProgramNodeId,
        slot: VarSlot,
        init: ProgramNodeId,
        update: ProgramNodeId,
        extract: Option<ProgramNodeId>,
        is_foreach: bool,
        input: EngineResult<'source>,
        cont: usize,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let init_input = input.try_clone().map_err(EngineRunError::Codec)?;
        let frame = if is_foreach {
            GraphFrame::ForeachInit {
                source,
                slot,
                update,
                extract,
                input,
                out_cont: cont,
            }
        } else {
            GraphFrame::ReduceInit {
                source,
                slot,
                update,
                input,
                out_cont: cont,
            }
        };
        self.push_frame(frame);
        self.pending = Some(GraphTask::Eval {
            node: init,
            input: init_input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// The executor-recognized keyed-collect shape: a `reduce` over a source
    /// that files each row into an object under the row's key — the INDEX
    /// lowering (`reduce s as $row ({}; reduce ($row|idx|tostring) as $k
    /// (.; .[$k] = $row))`) and the user-written spellings it inlines to
    /// (`def INDEX …` bodies, `reduce $x[] as $r ({}; .[$r.k] = $r)`).
    ///
    /// Returns the KEY GRAPH when the reduce is exactly the shape: the outer
    /// init is the empty object literal, and the update is ONE `.[$k] = $row`
    /// set — either the builtin lowering's inner reduce (identity init, the
    /// key already bound in the inner slot) or the generic `=` form (RHS
    /// bound outside the Modify, the key bound outside the path stage). The
    /// RHS must be a BARE read of the loop's row — a general RHS could raise
    /// or read the accumulator, which this drive must not swallow or
    /// misread — and the key graph must end in the BUILTIN `tostring` or a
    /// string literal, which is the only guarantee that every key is a
    /// string (a non-string key would need the generic `DynVar`'s
    /// index-class raise, which this drive does not invent).
    ///
    /// Any divergence returns `None` and the generic drive runs, so the fast
    /// path can never change an answer; it only makes the recognized shape
    /// cheaper.
    /// Classifies every Reduce node once after fusion. Runtime reads the
    /// stored key graph; it does not re-walk the update.
    pub(crate) fn mark_keyed_collects(nodes: &mut [ProgramNode]) {
        for index in 0..nodes.len() {
            let ProgramNode::Reduce { slot, init, update, .. } = nodes[index] else {
                continue;
            };
            let keys = Self::keyed_collect_keys(nodes, update, slot, init);
            if let ProgramNode::Reduce { keyed_collect, .. } = &mut nodes[index] {
                *keyed_collect = keys;
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one shape table with two spelling arms (builtin reduce / generic `=`); splitting \
                  it would hide which spelling is covered"
    )]
    pub(crate) fn keyed_collect_keys(
        nodes: &[ProgramNode],
        update: ProgramNodeId,
        outer_slot: VarSlot,
        outer_init: ProgramNodeId,
    ) -> Option<ProgramNodeId> {
        // The outer init must be exactly the empty object literal: the
        // specialized drive skips evaluating it (an empty literal has no
        // effect to preserve) and starts the builder empty.
        match nodes.get(outer_init.index())? {
            ProgramNode::Stage {
                start: StageStart::Literal(Value::Object(object)),
                steps,
            } if steps.is_empty() && object.is_empty() => {}
            _ => return None,
        }
        let keys = match nodes.get(update.index())? {
            // The builtin INDEX lowering: the update is the inner reduce.
            ProgramNode::Reduce {
                source: keys,
                slot: key_slot,
                init: inner_init,
                update: modify,
                keyed_collect: _,
            } => {
                // The inner init is the identity stage (the accumulator
                // passes through unchanged, which the builder-as-state
                // reproduces).
                match nodes.get(inner_init.index())? {
                    ProgramNode::Stage {
                        start: StageStart::Current,
                        steps,
                    } if steps.is_empty() => {}
                    _ => return None,
                }
                // The inner update is one `.[$k] = $row` set: a single
                // dynamic-index path on the state root, storing the loop's
                // row directly.
                match nodes.get(modify.index())? {
                    ProgramNode::Modify {
                        paths,
                        update: stored,
                        mode,
                    } if *mode == ModifyMode::Set => {
                        if !Self::single_dyn_var_path(nodes, *paths, *key_slot) {
                            return None;
                        }
                        match nodes.get(stored.index())? {
                            ProgramNode::Stage {
                                start: StageStart::Variable(slot),
                                steps,
                            } if *slot == outer_slot && steps.is_empty() => {}
                            _ => return None,
                        }
                    }
                    _ => return None,
                }
                *keys
            }
            // The generic `=` form (user-written defs and reduces): the RHS
            // is bound outside the Modify, and the dynamic index binds its
            // key outside the path stage.
            ProgramNode::Bind {
                source,
                slot: rhs_slot,
                body,
                frame: _,
            } => {
                // The RHS must be a BARE read of the loop's row. A general
                // input-free RHS would still be able to RAISE (`.x` on a
                // non-object row), and the generic `=` runs it with dot =
                // the accumulator — a raise this drive must not swallow.
                // The bare-slot spelling covers the entire `= $row` idiom
                // and cannot raise.
                match nodes.get(source.index())? {
                    ProgramNode::Stage {
                        start: StageStart::Variable(slot),
                        steps,
                    } if *slot == outer_slot && steps.is_empty() => {}
                    _ => return None,
                }
                let ProgramNode::Modify {
                    paths,
                    update: stored,
                    mode,
                } = nodes.get(body.index())?
                else {
                    return None;
                };
                if *mode != ModifyMode::Set {
                    return None;
                }
                // The paths generator is `KEY-STREAM as $k | .[$k]`.
                let ProgramNode::Bind {
                    source: key_stream,
                    slot: key_slot,
                    body: path_stage,
                    frame: _,
                } = nodes.get(paths.index())?
                else {
                    return None;
                };
                if !Self::single_dyn_var_path(nodes, *path_stage, *key_slot) {
                    return None;
                }
                match nodes.get(stored.index())? {
                    ProgramNode::Stage {
                        start: StageStart::Variable(slot),
                        steps,
                    } if *slot == *rhs_slot && steps.is_empty() => {}
                    _ => return None,
                }
                *key_stream
            }
            _ => return None,
        };
        // The key graph and its STRING GUARANTEE. The drive inserts keys
        // through `ObjectKey`, so a key that is not a string would need the
        // generic DynVar's index-class raise — which this drive must not
        // invent. The guarantee comes from the graph's shape: the key
        // graph's OUTPUT chain ends in the builtin tostring, or the graph
        // is a bare string literal. Anything else (a field read like
        // `.name`, whose type is unknown) DECLINES to the generic drive.
        //
        // The graph's root must read the outer binder — never the eval
        // input the specialized drive supplies — so the upstream spine
        // bottoms at a `Variable($row)` stage (the optimizer fuses
        // `$row | idx` into one stage, so the bottom stage may carry the
        // key steps; they are opaque). The OUTPUT chain is walked
        // body-first: each FlatMap's body is the next output stage, and the
        // walk must end at the builtin tostring call or a string literal.
        let tostring = jqf_builtins::registry::resolve_builtin("tostring", 0)?;
        // A bare string literal is already the whole key graph (its output
        // is the literal itself).
        if let ProgramNode::Stage {
            start: StageStart::Literal(Value::String(_)),
            steps,
        } = nodes.get(keys.index())?
            && steps.is_empty()
        {
            return Some(keys);
        }
        let ProgramNode::FlatMap {
            upstream: subject,
            body: rendered,
        } = nodes.get(keys.index())?
        else {
            return None;
        };
        let mut spine = *subject;
        loop {
            match nodes.get(spine.index())? {
                ProgramNode::FlatMap { upstream, .. } => spine = *upstream,
                ProgramNode::Stage {
                    start: StageStart::Variable(slot),
                    ..
                } if *slot == outer_slot => break,
                _ => return None,
            }
        }
        let mut tail = *rendered;
        loop {
            match nodes.get(tail.index())? {
                ProgramNode::FlatMap { body, .. } => tail = *body,
                ProgramNode::Call { overload, args, .. } => {
                    if *overload == tostring.id && args.is_empty() {
                        return Some(keys);
                    }
                    return None;
                }
                ProgramNode::Stage {
                    start: StageStart::Literal(Value::String(_)),
                    steps,
                } if steps.is_empty() => return Some(keys),
                _ => return None,
            }
        }
    }

    /// Whether `node` is a path stage of exactly `.[$slot]`: a `Current`
    /// stage whose single step is the dynamic index by `slot`.
    pub(crate) fn single_dyn_var_path(nodes: &[ProgramNode], node: ProgramNodeId, slot: VarSlot) -> bool {
        match nodes.get(node.index()) {
            Some(ProgramNode::Stage {
                start: StageStart::Current,
                steps,
            }) => {
                steps.len() == 1
                    && matches!(
                        steps[0].access(),
                        StepAccess::DynVar(bound) if *bound == slot
                    )
            }
            _ => false,
        }
    }

    /// Starts the keyed-collect drive for a recognized shape: the state is an
    /// in-progress [`ObjectBuilder`] (never a materialized Value), the source
    /// drives the outer stream, and each item's key graph feeds the builder
    /// through its incremental index — no path array, no seed, no Modify
    /// frames, no take-and-rehold of a growing object.
    pub(crate) fn eval_keyed_collect(
        &mut self,
        source: ProgramNodeId,
        slot: VarSlot,
        keys: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
    ) -> Option<EngineResult<'source>> {
        let builder = ObjectBuilder::new();
        self.push_frame(GraphFrame::KeyedCollectSource {
            slot,
            keys,
            builder,
            out_cont: cont,
        });
        self.pending = Some(GraphTask::Eval {
            node: source,
            input,
            cont: self.frames.len(),
        });
        None
    }

    /// Consumes one keyed-collect source item: bind it into the loop's slot,
    /// then drive the key graph (its outputs feed the builder). The key graph
    /// reads its row from the slot — the shape check pins that — so the eval
    /// input is only the slot's value re-borrowed, never the accumulator.
    pub(crate) fn consume_keyed_source(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let (slot, keys, frame) = match self.frames.as_mut_slice().get_mut(index) {
            Some(GraphFrame::KeyedCollectSource { slot, keys, .. }) => {
                (*slot, *keys, GraphFrame::KeyedCollectKey { source_frame: index })
            }
            _ => {
                return Err(internal_contract(
                    "keyed collect source frame kind changed under capture",
                ));
            }
        };
        let keys_input = value.try_clone().map_err(EngineRunError::Codec)?;
        self.write_slot(slot, value)?;
        self.push_frame(frame);
        self.pending = Some(GraphTask::Eval {
            node: keys,
            input: keys_input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Consumes one key output for a keyed collect: materialize the key (a
    /// string by the shape's tostring pin), insert the row bound in the
    /// source's slot under it — first position, last value, through the
    /// builder's incremental index — and stay for the next key.
    pub(crate) fn consume_keyed_key(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let source_frame = match self.frames.as_slice().get(index) {
            Some(GraphFrame::KeyedCollectKey { source_frame }) => *source_frame,
            _ => {
                return Err(internal_contract("keyed collect key frame kind changed under capture"));
            }
        };
        let key = self.materialize_capture(value, resources)?;
        let Value::String(key) = key else {
            // Unreachable: the shape check pins the BUILTIN tostring tail,
            // whose outputs are strings by construction. Never a silent
            // fallback — a classifier bug must fail loud.
            return Err(internal_contract("keyed collect received a non-string key"));
        };
        let (slot, out_cont) = match self.frames.as_slice().get(source_frame) {
            Some(GraphFrame::KeyedCollectSource { slot, out_cont, .. }) => (*slot, *out_cont),
            _ => {
                return Err(internal_contract(
                    "keyed collect source frame kind changed under insert",
                ));
            }
        };
        self.raise_cont = out_cont;
        let key = jqf_data::ObjectKey::try_from_str(key.as_str()).map_err(|_| EngineRunError::allocation_failure())?; // The row is the source item bound in the slot; each key of a
        // multi-key row inserts its own clone.
        let row = match self.env.as_slice().get(slot as usize) {
            Some(EngineResult::Owned(owned)) => owned.clone(),
            Some(EngineResult::Located(located)) => {
                let located = located.try_clone().map_err(EngineRunError::Codec)?;
                self.materialize_capture(EngineResult::Located(located), resources)?
            }
            None => {
                return Err(internal_contract("keyed collect read an unbound row slot"));
            }
        };
        let Some(GraphFrame::KeyedCollectSource { builder, .. }) = self.frames.as_mut_slice().get_mut(source_frame)
        else {
            return Err(internal_contract(
                "keyed collect source frame kind changed under borrow",
            ));
        };
        builder
            .try_insert_or_replace(key, row)
            .map_err(|_| EngineRunError::allocation_failure())?;
        Ok(None)
    }

    /// Consumes one init output for a loop: it becomes the initial state of ONE
    /// complete run over the source, driven now. A multi-output init therefore
    /// runs the whole loop once per init value, sequentially.
    ///
    /// The loop's retained outer dot drives the source, and it is re-BORROWED
    /// only while the init generator can still emit ([`Self::producer_spent_above`]);
    /// a spent init hands its last fold the MOVE. `INDEX` is the shape that needs
    /// it: its inner fold is `reduce KEYS as $k (.; .[$k] = $row)`, whose init is
    /// the OUTER accumulator, so a retained second handle would detach the whole
    /// index on every key written.
    pub(crate) fn consume_loop_init(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let state = self.hold_state(value, resources)?;
        let spent = self.producer_spent_above(index);
        let (source, input, frame) = match self.frames.as_mut_slice().get_mut(index) {
            Some(GraphFrame::ReduceInit {
                source,
                slot,
                update,
                input,
                out_cont,
            }) => (
                *source,
                take_or_borrow(input, spent)?,
                GraphFrame::ReduceSource {
                    slot: *slot,
                    update: *update,
                    state,
                    out_cont: *out_cont,
                },
            ),
            Some(GraphFrame::ForeachInit {
                source,
                slot,
                update,
                extract,
                input,
                out_cont,
            }) => (
                *source,
                take_or_borrow(input, spent)?,
                GraphFrame::ForeachSource {
                    slot: *slot,
                    update: *update,
                    extract: *extract,
                    state,
                    out_cont: *out_cont,
                },
            ),
            _ => {
                return Err(internal_contract("loop init frame kind changed under capture"));
            }
        };
        self.push_frame(frame);
        self.pending = Some(GraphTask::Eval {
            node: source,
            input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Consumes one source item for a loop: bind it into the loop's slot and
    /// drive UPDATE with dot = the state TAKEN out of the frame. Taking the state
    /// (leaving `null`) is exactly the law that an update emitting nothing nulls
    /// the state, and it avoids cloning the accumulator every iteration.
    pub(crate) fn consume_loop_source(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let (slot, update, frame) = match self.frames.as_mut_slice().get_mut(index) {
            Some(GraphFrame::ReduceSource {
                slot, update, state, ..
            }) => (
                *slot,
                *update,
                (state.take(), GraphFrame::ReduceUpdate { source_frame: index }),
            ),
            Some(GraphFrame::ForeachSource {
                slot,
                update,
                extract,
                state,
                out_cont,
            }) => {
                let (extract, out_cont) = (*extract, *out_cont);
                (
                    *slot,
                    *update,
                    (
                        state.take(),
                        GraphFrame::ForeachUpdate {
                            source_frame: index,
                            extract,
                            out_cont,
                        },
                    ),
                )
            }
            _ => {
                return Err(internal_contract("loop source frame kind changed under capture"));
            }
        };
        let (state, update_frame) = frame;
        self.write_slot(slot, value)?;
        self.push_frame(update_frame);
        self.pending = Some(GraphTask::Eval {
            node: update,
            input: EngineResult::owned(state),
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Consumes one `reduce` UPDATE output: it overwrites the fold state, so the
    /// LAST output wins and nothing is published mid-fold.
    pub(crate) fn consume_reduce_update(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let cell = self.hold_state(value, resources)?;
        let source_frame = match self.frames.as_slice().get(index) {
            Some(GraphFrame::ReduceUpdate { source_frame }) => *source_frame,
            _ => {
                return Err(internal_contract("reduce update frame kind changed under capture"));
            }
        };
        match self.frames.as_mut_slice().get_mut(source_frame) {
            Some(GraphFrame::ReduceSource { state, .. }) => {
                *state = cell;
                Ok(None)
            }
            _ => Err(internal_contract("reduce source frame kind changed under update")),
        }
    }

    /// Consumes one `foreach` UPDATE output — the CONSUMER-THAT-RE-EMITS arm: the
    /// output becomes the next state AND is published, directly for a 2-arg loop
    /// or through the extract (dot = the NEW state, `$x` still bound) for a 3-arg
    /// one. This is linear, not forking: the last output is simply the next state.
    pub(crate) fn consume_foreach_update(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let cell = self.hold_state(value, resources)?;
        let emission = cell.value.clone();
        let (source_frame, extract, out_cont) = match self.frames.as_slice().get(index) {
            Some(GraphFrame::ForeachUpdate {
                source_frame,
                extract,
                out_cont,
            }) => (*source_frame, *extract, *out_cont),
            _ => {
                return Err(internal_contract("foreach update frame kind changed under capture"));
            }
        };
        match self.frames.as_mut_slice().get_mut(source_frame) {
            Some(GraphFrame::ForeachSource { state, .. }) => *state = cell,
            _ => {
                return Err(internal_contract("foreach source frame kind changed under update"));
            }
        }
        match extract {
            None => self.route(EngineResult::owned(emission), out_cont, resources),
            Some(extract) => {
                self.pending = Some(GraphTask::Eval {
                    node: extract,
                    input: EngineResult::owned(emission),
                    cont: out_cont,
                });
                Ok(None)
            }
        }
    }

    /// Sizes the env vector to the program's declared slot count, once.
    ///
    /// A binder-free program (`slots == 0`) never reaches a slot write, so its run
    /// never grows the env vector at all — the identity/pure-path cost floor is
    /// untouched by this vertical.
    ///
    /// # Errors
    ///
    /// A refused reservation raises `allocation_failure` instead of letting
    /// the growth abort, matching every other env-growth site.
    pub(crate) fn ensure_env(&mut self) -> Result<(), EngineRunError> {
        let slots = usize::try_from(self.slots).unwrap_or(usize::MAX);
        if self.env.len() >= slots {
            return Ok(());
        }
        self.env
            .try_reserve(slots - self.env.len())
            .map_err(|_| EngineRunError::allocation_failure())?;
        self.env.resize_with(slots, || EngineResult::owned(Value::Null));
        Ok(())
    }

    /// Writes one bound result into `slot` in the representation its source
    /// emitted — a located handle binds ZERO-COPY, an owned value binds as-is.
    pub(crate) fn write_slot(&mut self, slot: VarSlot, value: EngineResult<'source>) -> Result<(), EngineRunError> {
        self.ensure_env()?;
        let index = slot as usize;
        let cell = self
            .env
            .as_mut_slice()
            .get_mut(index)
            .ok_or_else(|| internal_contract("bind wrote an env slot outside the sized env"))?;
        *cell = value;
        Ok(())
    }

    /// Materializes one loop state value for the state register.
    pub(crate) fn hold_state(
        &mut self,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<StateCell, EngineRunError> {
        let value = self.materialize_uncharged(value, resources)?;
        Ok(StateCell { value })
    }
}
