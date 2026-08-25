//! Graph-machine seed, advance, and node-dispatch entry.
//!
//! One job: start a [`super::GraphMachine`], step its pending task, and
//! dispatch one arena node onto the evaluator that owns that family.
//! Bind/fold consumers, path-family handoff, and emission routing live in
//! their own siblings.

#[allow(clippy::wildcard_imports)]
use super::*;

impl<'program, 'source> GraphMachine<'program, 'source> {
    /// Seeds the graph machine to evaluate `root` over `input`, applying
    /// `entry_offset` at the `entry` stage the codec pushdown resolved.
    #[allow(
        clippy::too_many_arguments,
        reason = "the seed keeps every arena, boundary, env, and table ownership boundary explicit"
    )]
    pub(crate) fn seed(
        nodes: &'program [ProgramNode],
        root: ProgramNodeId,
        entry: ProgramNodeId,
        entry_offset: usize,
        slots: u32,
        scans: &'program [CorrelatedScan],
        topk: &'program [PartialSort],
        anti: &'program [AntiJoinScan],
        input: EngineResult<'source>,
    ) -> Result<Self, EngineRunError> {
        // The fact-write span authority is retained HERE or not at all: a
        // refused retain raises now, so a later fact write never blames
        // "non-located input" for what was an allocation failure.
        let origin = match &input {
            EngineResult::Located(located)
                if nodes.iter().any(|node| matches!(node, ProgramNode::FactAssign { .. })) =>
            {
                Some(located.try_clone().map_err(EngineRunError::Codec)?)
            }
            _ => None,
        };
        Ok(Self {
            nodes,
            pending: Some(GraphTask::Eval {
                node: root,
                input,
                cont: 0,
            }),
            frames: Vec::new(),
            entry,
            entry_offset,
            slots,
            env: Vec::new(),
            memo: None,
            fact_deltas: Vec::new(),
            origin,
            workspace: None,
            raise_cont: 0,
            callable_depth: 0,
            tail_rounds: 0,
            scans,
            topk,
            anti,
            joins: Vec::new(),

            user_indexes: Vec::new(),
            cursors: Vec::new(),
            json_facts_cache: jqf_builtins::registry::builtins::facts::JsonFactsCache::default(),

            path: None,
            done: false,
            queued_output: None,
            iterations_done: 0,
            iteration_cap: None,
            arg_reuse: None,
            body_reuse: None,
            arg_frames: Vec::new(),
            slot_scratch: Vec::new(),
            tail_read_body: None,
            tail_read_slots: Vec::new(),
            filter_env: Vec::new(),
        })
    }

    /// Takes a nested argument machine from the reuse slot, or seeds one.
    ///
    /// The seed threads this machine's top-k table so a partial-sort shape
    /// inside a builtin filter argument (`limit(3; sort_by(.x) | .[0:2])`)
    /// takes the bounded-heap route instead of the full-sort floor. The scan
    /// and anti-join tables stay EMPTY here, deliberately: their warm-up is
    /// runtime state built across REPEATED scans of one field
    /// (`dispatch.rs` join warm-up), which a per-call argument machine —
    /// seeded, driven, pooled — never reaches, and `arg_reuse` pooling would
    /// make that warm-up law ambiguous anyway (`reseed` retakes no table
    /// parameters, so a pooled machine keeps its ORIGINAL seed's tables).
    /// Do not "fix" scans/anti symmetrically with topk: the recognizer is
    /// stateless, so threading it is safe; the join tables are not.
    pub(crate) fn take_arg_machine(
        &mut self,
        filter: ProgramNodeId,
        input: Value,
    ) -> Result<Box<GraphMachine<'program, 'source>>, EngineRunError> {
        if let Some(mut machine) = self.arg_reuse.take() {
            machine.reseed(filter, EngineResult::owned(input));
            Ok(machine)
        } else {
            let mut machine = Box::new(GraphMachine::seed(
                self.nodes,
                filter,
                filter,
                0,
                self.slots,
                // Scans/anti intentionally empty — see the doc above.
                &[],
                self.topk,
                &[],
                EngineResult::owned(input),
            )?);
            machine.iteration_cap = self.iteration_cap;
            Ok(machine)
        }
    }

    /// Returns an argument machine to the reuse slot. A second machine (a
    /// nested argument that completed while this one was already pooled) is
    /// dropped — one live reuse slot is the whole win.
    pub(crate) fn put_arg_machine(&mut self, machine: Box<GraphMachine<'program, 'source>>) {
        if self.arg_reuse.is_none() {
            self.arg_reuse = Some(machine);
        }
    }

    /// Takes a nested callable-body machine from the reuse slot, or seeds
    /// one. The body drive seeds with THIS machine's scan/top-k/anti tables
    /// (unlike [`Self::take_arg_machine`]'s deliberately empty scans/anti),
    /// and `reseed` retakes no table parameters — every pooled machine was
    /// originally seeded from the same parent tables, so the references stay
    /// identical across reuse.
    pub(crate) fn take_body_machine(
        &mut self,
        body: ProgramNodeId,
        input: EngineResult<'source>,
    ) -> Result<Box<GraphMachine<'program, 'source>>, EngineRunError> {
        if let Some(mut machine) = self.body_reuse.take() {
            machine.reseed(body, input);
            Ok(machine)
        } else {
            let mut machine = Box::new(GraphMachine::seed(
                self.nodes, body, body, 0, self.slots, self.scans, self.topk, self.anti, input,
            )?);
            machine.iteration_cap = self.iteration_cap;
            Ok(machine)
        }
    }

    /// Returns a completed callable-body machine to the reuse slot. A second
    /// machine completing while one is already pooled is dropped — one live
    /// reuse slot per parent, exactly like [`Self::put_arg_machine`].
    pub(crate) fn put_body_machine(&mut self, machine: Box<GraphMachine<'program, 'source>>) {
        if self.body_reuse.is_none() {
            self.body_reuse = Some(machine);
        }
    }

    /// Copies the parent's query-time fact overlay into a nested machine.
    /// Call this before every nested `advance`, not only at seed: a parent
    /// write between two child polls must be visible on the next resume, and
    /// the matching adopt would otherwise copy a stale child snapshot over it.
    pub(crate) fn inherit_fact_deltas(&mut self, parent: &Self) {
        self.fact_deltas.clone_from(&parent.fact_deltas);
    }

    /// Copies a nested machine's fact overlay back onto the parent. A write
    /// inside `def f: .a.@comment = ["x"]; f | .a.@comment` lands on the
    /// child; the parent read between child items (and after Complete) must
    /// see it.
    pub(crate) fn adopt_child_fact_deltas(&mut self, child: &Self) {
        self.fact_deltas.clone_from(&child.fact_deltas);
    }

    /// Resets a machine to evaluate `root` over `input`, keeping env and
    /// workspace allocations.
    pub(crate) fn reseed(&mut self, root: ProgramNodeId, input: EngineResult<'source>) {
        self.pending = Some(GraphTask::Eval {
            node: root,
            input,
            cont: 0,
        });
        self.frames.clear();
        self.entry = root;
        self.entry_offset = 0;
        self.memo = None;
        self.fact_deltas.clear();
        self.origin = None;
        self.raise_cont = 0;
        self.callable_depth = 0;
        self.tail_rounds = 0;
        self.joins.clear();
        self.user_indexes.clear();
        self.json_facts_cache.clear();
        self.cursors.clear();
        self.path = None;
        self.done = false;
        self.queued_output = None;
        self.iterations_done = 0;
        self.filter_env.clear();
    }

    /// Advances the stream by at most one ordered item, honoring cooperative
    /// pauses through the advance loop's per-task admissions.
    pub(crate) fn poll(&mut self, resources: &mut ResourceContext<'_>) -> Result<RunPoll<'source>, EngineRunError> {
        if self.done {
            return Ok(RunPoll::Complete);
        }
        self.advance(resources)
    }

    /// Drives the continuation state to the next ordered item, admitting one
    /// work transition per frame task and honoring cooperative pauses.
    ///
    /// The admission lives in the LOOP, not at the poll boundary, so a
    /// collecting drive — `[expr]`, object construction, a fold over a
    /// structural source — is metered per element instead of running its
    /// whole collect under the single transition a per-poll admission would
    /// grant. That is the sampling law: the ledger's decision points
    /// (each collect push, member insert, and loop step IS one) are where the
    /// host governor observes the physical footprint, and the work-slice
    /// boundary every 4096 transitions is where the cooperative control check
    /// fires. A refusal surfaces as `RunPoll::Pending`; the machine's state is
    /// entirely in `pending` and the frame stack, so a re-poll resumes exactly
    /// where the slice ran out. Nested machines driven directly from within
    /// this loop (`CallableBody`, `PathWalkEmit`) surface their own Pending
    /// one iteration later, when this loop's next admission observes the
    /// exhausted slice.
    pub(crate) fn advance(&mut self, resources: &mut ResourceContext<'_>) -> Result<RunPoll<'source>, EngineRunError> {
        loop {
            match resources.admit_work_transition() {
                Ok(WorkAdmission::Granted(_)) => {}
                Ok(WorkAdmission::Pending) => return Ok(RunPoll::Pending),
                Err(error) => return Err(EngineRunError::from(EngineError::Control(error))),
            }
            admit_iteration(&mut self.iterations_done, self.iteration_cap)?;
            if let Some(task) = self.pending.take() {
                match self.step_task(task, resources) {
                    Ok(Some(item)) => return Ok(RunPoll::Item(item)),
                    Ok(None) => {}
                    Err(error) => self.handle_raise(error, resources)?,
                }
                continue;
            }
            match self.advance_frame(resources) {
                Ok(true) => {}
                Ok(false) => {
                    self.done = true;
                    return Ok(RunPoll::Complete);
                }
                Err(error) => self.handle_raise(error, resources)?,
            }
        }
    }

    /// The RAISE-CLAMP target for the frame at `index`, or `None` when the
    /// frame is not an operator boundary.
    ///
    /// Every frame whose operator evaluates a subexpression ABOVE the frame (a
    /// pipe's upstream, a binary's operands, a binder's source or body, a
    /// fold's init/source/update, an object member's key or value, a select's
    /// predicate, a collect's body, a generator's bound argument) carries — or
    /// names, through a sibling frame — the position where that operator's OWN
    /// result routes. [`Self::handle_raise`]'s walk clamps the raise
    /// continuation to that position as it descends past the frame, so a raise
    /// escaping the region's own barriers routes as a failure of the whole
    /// operator and cannot be caught by a stale barrier belonging to an
    /// already-run sibling. A frame that is not an operator boundary (a
    /// producer, a barrier, a comma anchor) returns `None`.
    pub(crate) fn raise_boundary(&self, index: usize) -> Option<usize> {
        match &self.frames.as_slice()[index] {
            GraphFrame::FlatMapBody { out_cont, .. }
            | GraphFrame::BinaryRight { out_cont, .. }
            | GraphFrame::BinaryLeft { out_cont, .. }
            | GraphFrame::ConcatPart { out_cont, .. }
            | GraphFrame::LogicalLeft { out_cont, .. }
            | GraphFrame::LogicalRight { out_cont }
            | GraphFrame::ConditionalBody { out_cont, .. }
            | GraphFrame::AlternativeLeft { out_cont, .. }
            | GraphFrame::SelectBody { out_cont, .. }
            | GraphFrame::BindBody { out_cont, .. }
            | GraphFrame::EngineBindBody { out_cont, .. }
            | GraphFrame::EngineGen { out_cont, .. }
            | GraphFrame::ReduceInit { out_cont, .. }
            | GraphFrame::ReduceSource { out_cont, .. }
            | GraphFrame::ForeachInit { out_cont, .. }
            | GraphFrame::ForeachSource { out_cont, .. }
            | GraphFrame::ForeachUpdate { out_cont, .. }
            | GraphFrame::KeyedCollectSource { out_cont, .. }
            | GraphFrame::KeyedStreamSource { out_cont, .. }
            | GraphFrame::Counted { out_cont, .. }
            | GraphFrame::ErrorArg { out_cont }
            | GraphFrame::Collect { out_cont, .. }
            | GraphFrame::CountCollect { out_cont, .. }
            | GraphFrame::ObjectDest { out_cont, .. }
            | GraphFrame::MathArgs { out_cont, .. }
            | GraphFrame::MathCompute { out_cont, .. }
            | GraphFrame::ArgumentAnswer { out_cont, .. }
            | GraphFrame::PathSetValue { out_cont, .. }
            | GraphFrame::PathSetPair { out_cont, .. }
            | GraphFrame::Walk { out_cont, .. }
            | GraphFrame::MapValues { out_cont, .. }
            | GraphFrame::KeyedCollect { out_cont, .. }
            | GraphFrame::ModifyFold { out_cont, .. }
            | GraphFrame::PathEmit { out_cont, .. }
            | GraphFrame::PathPublish { out_cont, .. }
            | GraphFrame::InputsEmit { out_cont }
            | GraphFrame::SelectorEmit { out_cont, .. }
            | GraphFrame::PathWalkEmit { out_cont, .. }
            | GraphFrame::TostreamEmit { out_cont, .. } => Some(*out_cont),
            // The frames that NAME the operator that owns them, reached through
            // the named frame's own boundary (recursion terminates: the named
            // frames carry out_cont directly and name nothing).
            GraphFrame::ObjectKey { dest, .. } | GraphFrame::ObjectValue { dest, .. } => self.raise_boundary(*dest),
            GraphFrame::ReduceUpdate { source_frame } | GraphFrame::KeyedCollectKey { source_frame } => {
                self.raise_boundary(*source_frame)
            }
            GraphFrame::ModifyUpdate { fold_frame, .. } => self.raise_boundary(*fold_frame),
            // Every generator state carries the generator's own out-continuation:
            // a raise escaping a bound argument's evaluation routes as the
            // generator call's failure, never past a stale sibling barrier.
            GraphFrame::Generate { state } => Some(state.out_cont()),
            _ => None,
        }
    }

    /// The innermost `Stream`-mode path-family barrier whose out-continuation
    /// encloses `raise_cont`, or `None`.
    pub(crate) fn path_publish_barrier(&self, raise_cont: usize) -> Option<usize> {
        for index in (0..self.frames.len()).rev() {
            if let GraphFrame::PathPublish { finish, out_cont, .. } = &self.frames[index]
                && matches!(finish, PathFinish::Stream)
                && *out_cont <= raise_cont
            {
                return Some(index);
            }
        }
        None
    }

    /// Catches one path-family body failure at its barrier: pop the barrier
    /// and every intervening frame (store the error, re-push, and let the
    /// barrier's advance hand the collected values and the failure to the
    /// emitter).
    pub(crate) fn catch_path_publish(
        &mut self,
        index: usize,
        error: EngineRunError,
        resources: &ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        while self.frames.len() > index + 1 {
            self.pop_frame_releasing(resources);
        }
        let mut frame = self
            .frames
            .pop()
            .ok_or_else(|| internal_contract("path publish barrier vanished"))?;
        match &mut frame {
            GraphFrame::PathPublish { pending, .. } => *pending = Some(error),
            _ => {
                return Err(internal_contract("path publish frame kind changed under barrier"));
            }
        }
        self.push_frame(frame);
        self.pending = None;
        Ok(())
    }

    /// Routes a mid-execution error to the nearest enclosing `try` barrier, or
    /// tears the run down when it escapes.
    ///
    /// The raise diagnostic records at the current raise point first — the
    /// registry code, the payload-free kind and bounded operand, and the catch
    /// depth when a barrier absorbs it; a no-op under the `Off` policy.
    ///
    /// A catch-INELIGIBLE error ([`EngineRunError::Codec`]) tears THROUGH every
    /// barrier. A catch-eligible error unwinds LIFO to the nearest
    /// [`GraphFrame::Try`]: the intervening frames (and the barrier itself) are
    /// popped — releasing their partial state and ledger charges — BEFORE the
    /// handler seeds (pop-before-seed, so a handler raise bypasses this barrier).
    /// A catchless barrier swallows the error (no output); a barrier with a
    /// handler seeds it with the caught VALUE at the barrier's out-continuation.
    /// With no enclosing barrier the error escapes: the run tears down and the
    /// error surfaces on this poll (its emitted prefix already stands).
    pub(crate) fn handle_raise(
        &mut self,
        error: EngineRunError,
        resources: &ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        // The path-family body's barrier catches its body's failures even
        // when NOT catch-eligible (a recursion/resource limit): the collected
        // prefix streams, then the failure raises. Any other non-catchable
        // failure tears through, exactly as before.
        if !error.is_catch_eligible() {
            if let Some(index) = self.path_publish_barrier(self.raise_cont) {
                self.catch_path_publish(index, error, resources)?;
                return Ok(());
            }
            self.tear_down(resources);
            return Err(error);
        }
        // A break carries the marker naming the label occurrence it exits;
        // any other raise leaves this `None` and only `Try` frames answer it.
        let break_slot = match &error {
            EngineRunError::Raised(value) => marker_slot(value),
            _ => None,
        };
        let mut raise_cont = self.raise_cont;
        let mut index = self.frames.len();
        while index > 0 {
            index -= 1;
            // The SIBLING-BOUNDARY clamp: a barrier
            // belonging to an operand that ALREADY RAN stays on the stack
            // while the operator evaluates its sibling operand on top of it
            // (a binary's left over the right's live producer, an `and`/`or`
            // right over the left's, one object member's value over the
            // previous member's, a fold's update over the source's, a
            // binder's BODY over the source's, a pipe's body over the
            // upstream's barrier). The sibling's own evaluation pushes
            // frames ABOVE that stale barrier, so its raises route at
            // positions the barrier positionally encloses
            // (`out_cont <= raise_cont`) and the barrier swallows a failure
            // the floor routes past it. The trim cannot reach this: the
            // already-run operand's producer is still LIVE (a `Choice` with
            // a member to fire, a `Generate`), so the barrier is not the
            // topmost spent frame.
            //
            // Every operator frame below marks where an evaluation region
            // ends and the already-run region begins, and carries (or
            // names) the operator's own out-continuation. A raise that
            // escapes the region's OWN barriers (which sit above the frame
            // and are tested unclamped) routes, past this point, as a
            // failure of the whole operator — so clamp the raise
            // continuation to the operator's position. A stale barrier's
            // out-continuation is above that position, so the enclosure test
            // skips it; an enclosing barrier's is at or below it, so it
            // still catches. The clamp ACCUMULATES with `min`: a nested
            // region's raise crosses the inner boundary first (its own
            // operator's position) and the outer boundary after (the
            // enclosing operator's position, which is the tighter bound).
            if let Some(boundary) = self.raise_boundary(index) {
                raise_cont = raise_cont.min(boundary);
            }
            // A `Label` answers ONLY its own slot's marker, and always
            // swallows. It shares this walk with `Try` on purpose: whichever
            // barrier is nearer the top wins, which is exactly the ordering
            // for a `try` inside versus outside a `label`.
            if let GraphFrame::Label { slot, .. } = &self.frames.as_slice()[index]
                && break_slot == Some(*slot)
            {
                while self.frames.len() > index {
                    self.pop_frame_releasing(resources);
                }
                // The label completes with no further output; the outputs it
                // already emitted stand. Resume from the frame below.
                self.pending = None;
                return Ok(());
            }
            // The path-family body's barrier: a `Stream`-mode drive
            // catches its body's failure (published after the collected
            // prefix); an `Assignment`-mode drive lets it propagate.
            if let GraphFrame::PathPublish { finish, out_cont, .. } = &self.frames.as_slice()[index] {
                if *out_cont > raise_cont || !matches!(finish, PathFinish::Stream) {
                    continue;
                }
                self.catch_path_publish(index, error, resources)?;
                return Ok(());
            }
            if let GraphFrame::Try { handler, out_cont } = &self.frames.as_slice()[index] {
                let (handler, out_cont) = (*handler, *out_cont);
                // A barrier catches only when it ENCLOSES the failed value: its
                // out-continuation is at or below the failure's routing position.
                // A `try` whose out-continuation is above the failure is a
                // downstream consumer's barrier the failure escaped — skip it.
                if out_cont > raise_cont {
                    continue;
                }
                // The barrier depth is measured BEFORE the pop: the loop below
                // pops to exactly `index`, so reading `frames.len() - index`
                // after it would always be zero.
                let caught_depth = self.frames.len() - index;
                // Pop-before-seed: drop the barrier and every intervening frame
                // (releasing their state + charges) so a handler raise cannot be
                // caught by this same barrier.
                while self.frames.len() > index {
                    self.pop_frame_releasing(resources);
                }
                match handler {
                    None => {
                        // Catchless `try`: swallow — resume from the frame below.
                        emit_raise_diagnostic(resources, &error, Some(u32::try_from(caught_depth).unwrap_or(u32::MAX)));
                        self.pending = None;
                        return Ok(());
                    }
                    Some(handler) => {
                        emit_raise_diagnostic(resources, &error, Some(u32::try_from(caught_depth).unwrap_or(u32::MAX)));
                        let value = error.into_caught().inspect_err(|_| {
                            self.tear_down(resources);
                        })?;
                        self.pending = Some(GraphTask::Eval {
                            node: handler,
                            input: EngineResult::owned(value),
                            cont: out_cont,
                        });
                        return Ok(());
                    }
                }
            }
        }
        self.tear_down(resources);
        Err(error)
    }

    /// Processes one pending task. Returns `Ok(Some(item))` when it produced an
    /// ordered output, `Ok(None)` when it only reshaped the continuation, or an
    /// error that tears the run down.
    pub(crate) fn step_task(
        &mut self,
        task: GraphTask<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        match task {
            GraphTask::Eval { node, input, cont } => self.eval(node, input, cont, resources),
            GraphTask::Descend { node, value, at, cont } => self.descend_stage(node, value, at, cont, resources),
            GraphTask::Route { value, cont } => self.route(value, cont, resources),
        }
    }

    /// Evaluates one node over `input`, routing its outputs through `[0..cont]`.
    #[allow(
        clippy::too_many_lines,
        clippy::similar_names,
        reason = "one dispatch arm per arena node family: splitting it would hide the total \
                  node-to-evaluator mapping the routing law is read against; the Counted arm's \
                  count/kind locals beside the routing cont continuation are the countdown's \
                  own field names"
    )]
    pub(crate) fn eval(
        &mut self,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        match &self.nodes[node.index()] {
            ProgramNode::Stage { start, steps } => {
                // A stage with NO steps IS its start value: `.` is the input and
                // a bare literal is itself. `descend` over an empty step list
                // returns `Leaf(value)` on its first `steps.get(at)`, whatever
                // the offset — so the only work it can still do is materialize a
                // span-backed LOCATED root, which an owned value cannot need.
                // Taking the exit here skips a `StepScratch` build and two calls
                // per operand: every `. + 1` and `. > n` in a loop body is one.
                if steps.is_empty() {
                    match start {
                        StageStart::Current if matches!(input, EngineResult::Owned(_)) => {
                            return self.route(input, cont, resources);
                        }
                        StageStart::Literal(literal) => {
                            let mut value = literal.clone();
                            // A literal seed is FRESH: it must fail the
                            // register's allocation identity.
                            if self.path.is_some() {
                                value = path_register::box_computed(value);
                            }
                            return self.route(EngineResult::owned(value), cont, resources);
                        }
                        StageStart::Current | StageStart::Variable(_) => {}
                    }
                }
                // The ENTRY stage the codec pushdown resolved starts past the
                // pushed-down prefix; every other stage starts at step 0. A
                // literal-start stage ignores its input and its offset entirely.
                let offset = if node == self.entry { self.entry_offset } else { 0 };
                match start {
                    StageStart::Current => self.descend_stage(node, input, offset, cont, resources),
                    StageStart::Literal(literal) => {
                        let mut value = literal.clone();
                        if self.path.is_some() {
                            value = path_register::box_computed(value);
                        }
                        self.descend_stage(node, EngineResult::owned(value), 0, cont, resources)
                    }
                    // A variable-start stage ignores its input and navigates a
                    // BORROW of the slot value, cloning only the emitted leaf.
                    StageStart::Variable(slot) => {
                        let slot = *slot;
                        self.descend_variable_stage(node, slot, cont, resources)
                    }
                }
            }
            // `empty`: zero outputs, no navigation, no error. The driver simply
            // resumes the frame below.
            ProgramNode::Empty => Ok(None),
            // `~CONSTRUCTOR as ~x | BODY`: evaluate the constructor ONCE into a
            // suspended cursor machine, push the RAII release barrier, and run
            // the body once over the original input.
            ProgramNode::EngineBind { source, slot, body } => {
                let (source, slot, body) = (*source, *slot, *body);
                self.eval_engine_bind(source, slot, body, input, cont)
            }
            // `~x.next` / `~x.rest`: push the pull frame; its advance drives the
            // cursor machine. The node is only ever evaluated by the machine
            // whose cursor store holds the slot (the binding's own machine).
            ProgramNode::EnginePull { slot, kind } => {
                let (slot, rest) = (*slot, *kind == EnginePullKind::Rest);
                self.push_frame(GraphFrame::EnginePull {
                    slot,
                    rest,
                    out_cont: cont,
                });
                Ok(None)
            }
            // The cursor BODY: the machine whose root is this node (the nested
            // cursor machine `EngineBind` seeds) starts its phase drive here.
            // The ENCLOSING machine never evaluates it — the node is referenced
            // only as an `EngineBind` source, which seeds the nested machine
            // rather than evaluating it — so reaching this arm is always the
            // cursor machine's own drive.
            ProgramNode::EngineGenerator { .. } => self.eval_engine_generator(node, input),
            // The cursor BODY of `~rng($seed)`: the nested cursor machine's own
            // root, exactly like `EngineGenerator` — the enclosing machine only
            // ever seeds it through `EngineBind` and never evaluates it.
            ProgramNode::EngineRng { .. } => self.eval_engine_rng(node, input),
            ProgramNode::Bind {
                source,
                slot,
                body,
                frame,
            } => {
                let (source, slot, body, frame) = (*source, *slot, *body, *frame);
                self.eval_bind(source, slot, body, frame, input, cont)
            }
            ProgramNode::Reduce {
                source,
                slot,
                init,
                update,
                keyed_collect,
            } => {
                let (source, slot, init, update, keyed_collect) = (*source, *slot, *init, *update, *keyed_collect);
                // The recognized keyed-collect shape is classified once after
                // fusion and stored on the Reduce node. A load here, not a
                // re-walk of the update graph.
                if let Some(keys) = keyed_collect {
                    return Ok(self.eval_keyed_collect(source, slot, keys, input, cont));
                }
                self.eval_loop(source, slot, init, update, None, false, input, cont)
            }
            ProgramNode::Foreach {
                source,
                slot,
                init,
                update,
                extract,
            } => {
                let (source, slot, init, update, extract) = (*source, *slot, *init, *update, *extract);
                self.eval_loop(source, slot, init, update, extract, true, input, cont)
            }
            ProgramNode::Counted {
                source, count, kind, ..
            } => {
                let (source, slot, kind) = (*source, *count, *kind);
                self.eval_counted(source, slot, kind, input, cont, resources)
            }
            ProgramNode::CollectArray { body } => self.eval_collect(*body, input, cont, resources),
            ProgramNode::CountCollect { body } => self.eval_count_collect(*body, input, cont, resources),
            ProgramNode::ConstructObject { .. } => self.eval_construct_object(node, input, cont, resources),
            ProgramNode::Choice { left, right } => {
                let (left, right) = (*left, *right);
                // The right member runs over the SAME input: re-borrow it now
                // (fallible at the fork), leaving the original for the left member.
                let saved = input.try_clone().map_err(EngineRunError::Codec)?;
                self.push_frame(GraphFrame::Choice {
                    right,
                    input: saved,
                    cont,
                });
                self.pending = Some(GraphTask::Eval {
                    node: left,
                    input,
                    cont,
                });
                Ok(None)
            }
            ProgramNode::FlatMap { upstream, body } => {
                // The recognizer: a sort whose SOLE consumer is a constant-bound
                // head/tail slice is answered with the
                // bounded-heap partial sort instead of the full sort, when the
                // row's run-time side condition holds (a `sort_by` row declines
                // back to the floor over a non-ARRAY input, whose object arm
                // the partial sort cannot reproduce). Zero bytes have reached
                // the sink either way: the sort has not run yet.
                if let Some(row) = self.topk_row(node)
                    && self.take_partial_sort(row, &input)
                {
                    super::PARTIAL_SORT_ENGAGEMENTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    return self.eval_partial_sort(row, input, cont, resources);
                }
                // The streaming aggregate fusion: `[gen] | group_by(f)` /
                // `min_by(f)` / `max_by(f)` files each generated element
                // DIRECTLY into per-key accumulators instead of materializing
                // the array first. The published bytes are the same either way
                // (the drive's own doc pins the laws), so the fusion is
                // invisible above this line.
                if let Some((generator, mode, key_graph)) = GraphMachine::streamed_aggregate(self.nodes, node) {
                    super::STREAMED_KEYED_ENGAGEMENTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    return Ok(self.eval_keyed_stream(mode, generator, key_graph, input, cont));
                }
                let (upstream, body) = (*upstream, *body);
                self.push_frame(GraphFrame::FlatMapBody { body, out_cont: cont });
                // The upstream's leaves route to the FlatMapBody just pushed (the
                // last frame, index `frames.len() - 1`), so its ceiling is the new
                // frame count.
                let upstream_cont = self.frames.len();
                // A recognized correlated scan replaces its SOURCE ITERATION and
                // nothing else: the `FlatMapBody` above still runs the whole
                // `select` predicate over every child the index hands back, so
                // the consumer sees the same values in the same order either way.
                if let Some(slot) = self.scan_slot(node)
                    && matches!(
                        self.try_indexed_scan(slot, &input, upstream_cont, resources)?,
                        ScanRoute::Indexed
                    )
                {
                    return Ok(None);
                }
                self.pending = Some(GraphTask::Eval {
                    node: upstream,
                    input,
                    cont: upstream_cont,
                });
                Ok(None)
            }
            ProgramNode::Call { payload, .. } => self.eval_call(*payload, node, input, cont, resources),
            ProgramNode::CallDef {
                body,
                param_slots,
                filter_slots,
                args,
                filter_args,
                tail,
            } => {
                let (body, param_slots, filter_slots, args, filter_args, tail) =
                    (*body, param_slots, filter_slots, args, filter_args, *tail);
                self.eval_call_def(
                    body,
                    param_slots,
                    filter_slots,
                    args,
                    filter_args,
                    tail,
                    input,
                    cont,
                    resources,
                )
            }
            ProgramNode::CallFilter { slot } => {
                let slot = *slot;
                self.eval_call_filter(slot, input, cont, resources)
            }
            ProgramNode::Binary { op, left, right, shape } => {
                let (op, left, right, shape) = (*op, *left, *right, *shape);
                self.eval_binary(op, left, right, shape, input, cont, resources)
            }
            ProgramNode::Concat { .. } => self.eval_concat(node, input, cont, resources),
            ProgramNode::Conditional {
                condition,
                consequent,
                alternative,
            } => {
                let (condition, consequent, alternative) = (*condition, *consequent, *alternative);
                self.eval_conditional(condition, consequent, alternative, input, cont)
            }
            ProgramNode::Alternative { left, right } => {
                let (left, right) = (*left, *right);
                self.eval_alternative(left, right, input, cont, resources)
            }
            ProgramNode::Logical { operator, left, right } => {
                let (operator, left, right) = (*operator, *left, *right);
                self.eval_logical(operator, left, right, input, cont)
            }
            ProgramNode::Try { body, handler } => {
                let (body, handler) = (*body, *handler);
                Ok(self.eval_try(body, handler, input, cont, resources))
            }
            // A shared `?//` chain body is a pure indirection: evaluate it with
            // the current input as dot, and raises pass through unchanged to
            // the enclosing `Try` barrier.
            ProgramNode::ChainBody { body } => {
                let body = *body;
                self.eval(body, input, cont, resources)
            }
            ProgramNode::Label { slot, body } => {
                let (slot, body) = (*slot, *body);
                Ok(self.eval_label(slot, body, input, cont))
            }
            // `break $out` consumes its input, emits nothing, and raises the
            // marker naming its label. `raise_cont` is set first so the `Try`
            // half of the barrier walk applies its usual enclosure test.
            ProgramNode::Break { slot } => {
                let slot = *slot;
                drop(input);
                self.raise_cont = cont;
                Err(EngineRunError::Raised(break_marker(slot, resources)?))
            }
            ProgramNode::Modify { paths, update, mode } => {
                let (paths, update, mode) = (*paths, *update, *mode);
                self.eval_modify(paths, update, mode, input, cont, resources)
            }
            ProgramNode::FactAssign {
                paths,
                role,
                kind,
                selector,
                update,
                mode,
            } => {
                let (paths, update, mode) = (*paths, *update, *mode);
                self.eval_fact_assign(paths, role, kind, *selector, update, mode, input, cont, resources)
            }
        }
    }

    /// Evaluates `label $out | BODY` over `input`: push a `Label` barrier, then
    /// drive the body at the label's OWN out-continuation.
    ///
    /// The body routes with the label's original `cont` for the same reason a
    /// `try` body does — the barrier sits above that ceiling, so body emissions
    /// bypass it transparently and reach the consumers the label's own outputs
    /// would reach.
    pub(crate) fn eval_label(
        &mut self,
        slot: LabelSlot,
        body: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
    ) -> Option<EngineResult<'source>> {
        self.push_frame(GraphFrame::Label { slot });
        self.pending = Some(GraphTask::Eval {
            node: body,
            input,
            cont,
        });
        None
    }

    /// Evaluates a `try body [catch handler]` over `input`: push a `Try` barrier
    /// (transparent to body emissions), then drive the body over `input` at the
    /// `try`'s OWN out-continuation. Body leaves route past the barrier (it sits
    /// at/above their ceiling) exactly as the `try`'s outputs would; a raised error
    /// walks to this barrier in [`Self::handle_raise`].
    pub(crate) fn eval_try(
        &mut self,
        body: ProgramNodeId,
        handler: Option<ProgramNodeId>,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Option<EngineResult<'source>> {
        self.push_frame(GraphFrame::Try {
            handler,
            out_cont: cont,
        });
        // The try's BODY is the suppression region: the author
        // marked the sites inside it as expected. The frame's lifecycle
        // brackets the body (it is popped when the body finishes or a raise
        // unwinds to it), so the depth is entered here and exited wherever the
        // frame pops.
        resources.enter_mismatch_suppression();
        // The body routes with the `try`'s ORIGINAL out-continuation: like a
        // `Choice`, the barrier sits above that ceiling, so body emissions bypass
        // it transparently and reach the same consumers the `try`'s outputs would.
        self.pending = Some(GraphTask::Eval {
            node: body,
            input,
            cont,
        });
        None
    }
}
