//! User-defined calls, filter arguments, and owned-argument product.
//!
//! One job: drive `CallDef` / `CallFilter`, collect argument products, and
//! evaluate owned argument laws. [`super::super::GraphMachine::eval_call`] maps a
//! resolved builtin `Call` onto the family that owns that evaluator.

#[allow(clippy::wildcard_imports)]
use super::super::*;

impl<'program, 'source> GraphMachine<'program, 'source> {
    /// Drives `error/1`: raise its argument filter's FIRST output.
    ///
    /// The argument runs into an `ErrorArg` consumer rather than being evaluated
    /// here, because it is an ordinary filter over the same input and may fan
    /// out, pause, or raise on its own before it ever produces the value that
    /// becomes the error.
    pub(crate) fn eval_error_arg(
        &mut self,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let arg = match &self.nodes[node.index()] {
            ProgramNode::Call { args, .. } => *args
                .first()
                .ok_or_else(|| internal_contract("error/1 call with no argument"))?,
            _ => return Err(internal_contract("error dispatch over a non-call node")),
        };
        self.push_frame(GraphFrame::ErrorArg { out_cont: cont });
        self.pending = Some(GraphTask::Eval {
            node: arg,
            input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// One RECURSIVE definition call in the graph machine: evaluate the
    /// value-parameter arguments over the call's input (the `arg as $x` law,
    /// left-outer Cartesian), then run the shared body once per combination
    /// against a copied binder overlay with the parameters bound.
    ///
    /// The body is NOT evaluated eagerly. It runs in a nested graph machine
    /// held by a [`GraphFrame::CallableBody`] producer frame, one output per
    /// resumption, so a lazy consumer upstream of the call — a `limit`/`nth`
    /// countdown cut, a `first`-shape break — truncates the recursion at the
    /// exact moment it stops pulling. Eager evaluation could not: a recursive
    /// body materialized per level before its first output ran to the depth
    /// ceiling even for `limit(3; nat)`.
    ///
    /// A TAIL-position self-call (indicated by `tail`) trampolines instead of
    /// nesting: the call replaces this machine's own body drive — the overlay
    /// becomes the env and the body re-evaluates over the new dot at the
    /// call's own routing position — so the callable depth stays flat across
    /// arbitrarily many tail-call rounds. The marking is WIDER than classic
    /// tail position: the pass also marks a comma's LAST member (`def f: 1,
    /// f` — its outputs are the comma's final outputs) and a pipe's LEFT
    /// member (`def nat: 1, (nat | . + 1)` — the trampolined body emits into
    /// the same `FlatMapBody` the call's own outputs would enter, and the
    /// accumulated frames re-apply each level's continuation once), which is
    /// what keeps `limit(5000; f)` below the ceiling for both shapes. The
    /// frames the enclosing body evaluation still has open are NOT
    /// touched: they drain by the ordinary exhausted-frame pops once the
    /// replacement body's outputs finish routing, exactly as they would after
    /// the call's own outputs had. (Trimming them eagerly would be unsound: a
    /// tail call inside a `(a, b) | f` pipe has the comma's right member
    /// still owed above its routing position.) Every argument combination
    /// beyond the first is driven by a child-less `CallableBody` frame seeded
    /// below the body's routing position, so a comma argument (`f(1, 2)`)
    /// answers per output exactly as a nested call would.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "the call-def dispatch hands the run continuation and the requirement pieces to                   one evaluator; splitting them would hide which parameters the ceiling shares; the allocation collapse re-wrapped statements across extra lines without adding statements"
    )]
    pub(crate) fn eval_call_def(
        &mut self,
        body: ProgramNodeId,
        param_slots: &'program [VarSlot],
        filter_slots: &'program [FilterSlot],
        args: &[ProgramNodeId],
        filter_args: &[ProgramNodeId],
        tail: bool,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        let source_node = input.located().map(LocatedProduct::node);
        let root = self.materialize_capture(input, resources)?;
        let overlay = self.path_slots(&root, source_node, resources)?;
        let child_filters = self.capture_call_filters(&overlay, filter_slots, filter_args)?;
        let combos = self.eval_call_arg_product(args, &overlay, &root, resources)?;
        if tail {
            // The trampoline runs flat, so a divergent self-call
            // would never hit [`CALLABLE_DEPTH_LIMIT`] — count the rounds and
            // raise the same typed depth error past [`TAIL_ROUND_LIMIT`]. The
            // counter is per-MACHINE (reset on reseed), so a consumer that
            // cuts the recursion (`limit`, `first`) keeps the rounds bounded
            // by its own demand.
            self.tail_rounds = self.tail_rounds.saturating_add(1);
            if self.tail_rounds > TAIL_ROUND_LIMIT {
                return Err(tail_round_error());
            }
            // The tail trampoline replaces the machine's own body drive: bind
            // the FIRST argument combination over the caller-env snapshot,
            // install it as the env, and re-evaluate the body over the new dot
            // at the call's routing position. No nested machine is pushed, so
            // the callable depth does not grow across the rounds.
            //
            // The REMAINING combinations are not dropped: a `CallableBody`
            // frame seeded WITHOUT a child holds them, and when the
            // trampolined body's evaluation exhausts, the frame's ordinary
            // drive (the same combo loop the nesting path uses) seeds the next
            // combination's body. The frame sits exactly at `cont`, so the
            // body's outputs route past it (the routing walk covers
            // `[0, cont)`); it pops on a countdown cut, on a raise, or after
            // the last combination, exactly as a nested call's frame would.
            let mut combos = combos.into_iter();
            // A ZERO-combination tail call (`f(empty)`) produces zero outputs
            // AND terminates the trampolined continuation: the outstanding
            // body task belongs to that continuation, so it is consumed here
            // — leaving it armed would re-run the same zero-combination call
            // forever.
            let Some(first_combo) = combos.next() else {
                self.filter_env.clone_from(&child_filters);
                self.recycle_slot_scratch(overlay);
                self.tail_rounds = 0;
                return Ok(None);
            };
            if self.tail_read_body != Some(body) {
                self.tail_read_slots.clear();
                collect_read_slots(self.nodes, body, &mut self.tail_read_slots);
                self.tail_read_slots.extend(param_slots.iter().copied());
                self.tail_read_slots.sort_unstable();
                self.tail_read_slots.dedup();
                self.tail_read_body = Some(body);
            }
            for i in 0..self.tail_read_slots.len() {
                let slot = self.tail_read_slots[i];
                if param_slots.contains(&slot) {
                    continue;
                }
                match overlay.get(slot as usize) {
                    Some(Some(value)) => {
                        self.write_slot(slot, EngineResult::owned(value.clone()))?;
                    }
                    _ => {
                        self.write_slot(slot, EngineResult::owned(Value::Null))?;
                    }
                }
            }
            for (slot, value) in param_slots.iter().zip(&first_combo) {
                self.write_slot(*slot, EngineResult::owned(value.clone()))?;
            }
            let remaining: Vec<Vec<Value>> = combos.collect();
            self.filter_env.clone_from(&child_filters);
            self.push_frame(GraphFrame::CallableBody {
                body,
                root: root.clone(),
                overlay,
                param_slots,
                combos: remaining,
                combo: 0,
                depth: self.callable_depth + 1,
                child: None,
                out_cont: cont,
                filter_env: child_filters,
            });
            self.pending = Some(GraphTask::Eval {
                node: body,
                input: EngineResult::owned(root),
                cont,
            });
            return Ok(None);
        }
        if self.callable_depth >= CALLABLE_DEPTH_LIMIT {
            return Err(callable_depth_error());
        }
        self.push_frame(GraphFrame::CallableBody {
            body,
            root,
            overlay,
            param_slots,
            combos,
            combo: 0,
            depth: self.callable_depth + 1,
            child: None,
            out_cont: cont,
            filter_env: child_filters,
        });
        Ok(None)
    }

    /// Captures this call's filter arguments as closures over the current
    /// overlay and filter-env. Empty when the callable has no filter
    /// parameters and the caller had none.
    fn capture_call_filters(
        &self,
        overlay: &[Option<Value>],
        filter_slots: &[FilterSlot],
        filter_args: &[ProgramNodeId],
    ) -> Result<Vec<Option<FilterClosure>>, EngineRunError> {
        // The arity contract is checked BEFORE the empty fast path: an
        // argument/slot mismatch must raise even when both sides are empty-ish,
        // or the early return would silently accept it.
        if filter_slots.len() != filter_args.len() {
            return Err(internal_contract(
                "callable filter-argument count does not match its filter slots",
            ));
        }
        if filter_slots.is_empty() && self.filter_env.is_empty() {
            return Ok(Vec::new());
        }
        let overlay_rc = alloc::rc::Rc::new(overlay.to_vec());
        let captured = alloc::rc::Rc::new(self.filter_env.clone());
        let mut child = self.filter_env.clone();
        for (slot, node) in filter_slots.iter().zip(filter_args) {
            let index = *slot as usize;
            if child.len() <= index {
                child.resize(index + 1, None);
            }
            child[index] = Some(FilterClosure {
                node: *node,
                overlay: alloc::rc::Rc::clone(&overlay_rc),
                filters: alloc::rc::Rc::clone(&captured),
            });
        }
        Ok(child)
    }

    /// Evaluates one recursive-definition filter-parameter use: install the
    /// captured overlay and filter-env, drive the captured graph over the
    /// current input, and restore on pop.
    pub(crate) fn eval_call_filter(
        &mut self,
        slot: FilterSlot,
        input: EngineResult<'source>,
        cont: usize,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        let Some(closure) = self.filter_env.get(slot as usize).and_then(Option::clone) else {
            return Err(internal_contract("unbound filter parameter"));
        };
        self.ensure_env()?;
        let bound = usize::try_from(self.slots).map_err(|_| EngineRunError::allocation_failure())?;
        let mut new_env = Vec::new();
        new_env
            .try_reserve_exact(bound)
            .map_err(|_| EngineRunError::allocation_failure())?;
        for index in 0..bound {
            let value = match closure.overlay.get(index) {
                Some(Some(value)) => EngineResult::owned(value.clone()),
                _ => EngineResult::owned(Value::Null),
            };
            new_env.push(value);
        }
        let saved_env = core::mem::replace(&mut self.env, new_env);
        let saved_filters = core::mem::replace(&mut self.filter_env, (*closure.filters).clone());
        self.push_frame(GraphFrame::CallFilter {
            saved_env,
            saved_filters,
        });
        self.pending = Some(GraphTask::Eval {
            node: closure.node,
            input,
            cont,
        });
        Ok(None)
    }

    /// Evaluates every call argument. When each argument yields exactly one
    /// value the Cartesian product is that one combination — no
    /// `Vec<Vec<Value>>` of prefixes. Multi-value arguments take the
    /// generator product path.
    fn eval_call_arg_product(
        &mut self,
        args: &[ProgramNodeId],
        overlay: &[Option<Value>],
        root: &Value,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Vec<Vec<Value>>, EngineRunError> {
        if args.is_empty() {
            return Ok(vec![Vec::new()]);
        }
        let mut singles = Vec::new();
        singles
            .try_reserve_exact(args.len())
            .map_err(|_| EngineRunError::allocation_failure())?;
        let mut multi: Option<Vec<Vec<Value>>> = None;
        for argument in args {
            let outputs = self.eval_owned_argument(resources, overlay, *argument, root.clone())?;
            if outputs.is_empty() {
                return Ok(Vec::new());
            }
            if let Some(combos) = multi.as_mut() {
                let taken = core::mem::take(combos);
                *combos = cartesian_extend(&taken, &outputs)?;
                continue;
            }
            if outputs.len() == 1 {
                singles.push(outputs.into_iter().next().expect("len == 1"));
                continue;
            }
            let prefix = if singles.is_empty() {
                vec![Vec::new()]
            } else {
                vec![core::mem::take(&mut singles)]
            };
            multi = Some(cartesian_extend(&prefix, &outputs)?);
        }
        Ok(match multi {
            Some(combos) => combos,
            None => vec![singles],
        })
    }

    /// Evaluates one frozen argument graph, collecting owned outputs.
    ///
    /// A single-output binder-free graph runs inline on this machine (shared
    /// env, isolated frames). Anything else reseeds the pooled argument
    /// machine — the `PathWalkEmit` reuse/reseed pattern.
    pub(crate) fn eval_owned_argument(
        &mut self,
        resources: &mut ResourceContext<'_>,
        slots: &[Option<Value>],
        filter: ProgramNodeId,
        input: Value,
    ) -> Result<Vec<Value>, EngineRunError> {
        let (values, pending) = if argument_inline_eligible(self.nodes, filter) {
            self.eval_owned_argument_inline(resources, filter, input)?
        } else {
            let mut machine = self.take_arg_machine(filter, input)?;
            machine.inherit_fact_deltas(self);
            machine.filter_env.clone_from(&self.filter_env);
            let load_slots = graph_binds_slots(self.nodes, filter) || graph_reads_any_slot(self.nodes, filter);
            let loaded = (|| -> Result<(), EngineRunError> {
                if !load_slots {
                    return Ok(());
                }
                for (index, slot) in slots.iter().enumerate() {
                    if let Some(value) = slot {
                        machine.write_slot(
                            u32::try_from(index).map_err(|_| internal_contract("argument slot overflow"))?,
                            EngineResult::owned(value.clone()),
                        )?;
                    } else if let Some(cell) = machine.env.get_mut(index) {
                        *cell = EngineResult::owned(Value::Null);
                    }
                }
                if slots.len() < machine.env.len() {
                    for cell in &mut machine.env[slots.len()..] {
                        *cell = EngineResult::owned(Value::Null);
                    }
                }
                Ok(())
            })();
            if let Err(error) = loaded {
                self.put_arg_machine(machine);
                return Err(error);
            }
            let result = path_register::drain_owned_filter(&mut machine, resources);
            self.adopt_child_fact_deltas(&machine);
            self.put_arg_machine(machine);
            result?
        };
        if let Some(error) = pending {
            return Err(error);
        }
        Ok(values)
    }

    /// Drives `filter` on this machine with the caller's env, swapping the
    /// frame stack out so the argument cannot route through the caller's
    /// continuations.
    fn eval_owned_argument_inline(
        &mut self,
        resources: &mut ResourceContext<'_>,
        filter: ProgramNodeId,
        input: Value,
    ) -> Result<(Vec<Value>, Option<EngineRunError>), EngineRunError> {
        let saved_pending = self.pending.take();
        let saved_frames = core::mem::replace(&mut self.frames, core::mem::take(&mut self.arg_frames));
        let saved_done = self.done;
        let saved_entry = self.entry;
        let saved_entry_offset = self.entry_offset;
        let saved_raise_cont = self.raise_cont;
        let saved_tail_rounds = self.tail_rounds;
        let saved_callable_depth = self.callable_depth;
        let saved_path = self.path.take();
        let saved_memo = self.memo.take();
        self.pending = Some(GraphTask::Eval {
            node: filter,
            input: EngineResult::owned(input),
            cont: 0,
        });
        self.done = false;
        self.entry = filter;
        self.entry_offset = 0;
        self.raise_cont = 0;
        self.tail_rounds = 0;
        self.callable_depth = 0;
        self.path = None;
        let result = self.drain_inline_argument(resources);
        let mut scratch = core::mem::replace(&mut self.frames, saved_frames);
        scratch.clear();
        self.arg_frames = scratch;
        self.pending = saved_pending;
        self.done = saved_done;
        self.entry = saved_entry;
        self.entry_offset = saved_entry_offset;
        self.raise_cont = saved_raise_cont;
        self.tail_rounds = saved_tail_rounds;
        self.callable_depth = saved_callable_depth;
        self.path = saved_path;
        self.memo = saved_memo;
        result
    }

    fn drain_inline_argument(
        &mut self,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(Vec<Value>, Option<EngineRunError>), EngineRunError> {
        let mut out = Vec::new();
        loop {
            match self.poll(resources) {
                Ok(RunPoll::Item(EngineResult::Owned(value))) => {
                    out.try_reserve(1).map_err(|_| EngineRunError::allocation_failure())?;
                    out.push(value);
                }
                Ok(RunPoll::Item(located @ EngineResult::Located(_))) => {
                    let value = self.materialize_capture(located, resources)?;
                    out.try_reserve(1).map_err(|_| EngineRunError::allocation_failure())?;
                    out.push(value);
                }
                Ok(RunPoll::Complete) => return Ok((out, None)),
                Ok(RunPoll::Pending) => {
                    resources
                        .try_begin_next_cooperative_entry(4_096)
                        .map_err(|error| EngineRunError::Codec(jqf_codec_core::CodecError::from(error)))?;
                }
                Err(error) => return Ok((out, Some(error))),
            }
        }
    }

    /// One pure owned-value law: materialize the input once, compute, route.
    ///
    /// Six evaluators share this shape and differ only in `law`, so they share
    /// it here rather than repeating four lines each. The materialization is not
    /// avoidable for any of them: each publishes a value rebuilt from the WHOLE
    /// input, which is exactly the `Subtree` demand they declare.
    pub(crate) fn eval_owned_law(
        &mut self,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
        law: impl FnOnce(&Value, &ResourceContext<'_>) -> Result<Value, EngineRunError>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let subject = self.materialize_capture(input, resources)?;
        let mut value = law(&subject, resources)?;
        // A computed value is never the register's (the boxed arm lets the
        // allocation identity reject it — `path(.a | length)` over {"a":5}).
        if self.path.is_some() {
            value = path_register::box_computed(value);
        }
        let result = self.retain_owned(value);
        self.route(result, cont, resources)
    }

    /// One argument of a resolved `Call` node.
    pub(crate) fn call_arg(&self, node: ProgramNodeId, index: usize) -> Result<ProgramNodeId, EngineRunError> {
        match &self.nodes[node.index()] {
            ProgramNode::Call { args, .. } => args
                .get(index)
                .copied()
                .ok_or_else(|| internal_contract("a builtin call with too few arguments")),
            _ => Err(internal_contract("a builtin dispatch over a non-call node")),
        }
    }

    /// How many arguments a resolved `Call` node carries.
    pub(crate) fn call_arity(&self, node: ProgramNodeId) -> Result<usize, EngineRunError> {
        match &self.nodes[node.index()] {
            ProgramNode::Call { args, .. } => Ok(args.len()),
            _ => Err(internal_contract("a builtin dispatch over a non-call node")),
        }
    }

    /// Evaluates a `select(predicate)` over `input`: retain the original input in
    /// a `SelectBody` consumer frame, then drive the predicate graph over a
    /// re-borrow of that input. Each truthy predicate output re-borrows and routes
    /// the original input once; falsy outputs are dropped.
    pub(crate) fn eval_select(
        &mut self,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let predicate = match &self.nodes[node.index()] {
            ProgramNode::Call { args, .. } => *args
                .first()
                .ok_or_else(|| internal_contract("select call with no predicate argument"))?,
            _ => return Err(internal_contract("select dispatch over a non-call node")),
        };
        // The anti-join interception: a recognized negated-containment
        // predicate is answered from the sorted keyed index, queried for the
        // complement, instead of running the whole `isempty` scan. The element
        // is dropped when the index holds a key equal to the probe and routed
        // to the selection's out-continuation otherwise — the same output
        // sequence the naive predicate produces, per the row's soundness
        // argument. Every decline falls through to the ordinary select path.
        if let Some(slot) = self.anti_scan_slot(node)
            && self.try_anti_join(slot, &input, cont, resources)?
        {
            return Ok(None);
        }
        // The predicate is a frozen subexpression.
        self.path_enter_frozen();
        // The predicate runs over the SAME input the selection emits: re-borrow it
        // now (fallible at the fork), retaining the original for the emissions.
        let predicate_input = input.try_clone().map_err(EngineRunError::Codec)?;
        self.push_frame(GraphFrame::SelectBody { input, out_cont: cont });
        self.pending = Some(GraphTask::Eval {
            node: predicate,
            input: predicate_input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Consumes one predicate output for a `select`: on a truthy output, re-borrow
    /// the frame's original input and route it through the selection's
    /// out-continuation; on a falsy output, drop it and continue.
    pub(crate) fn consume_select(
        &mut self,
        index: usize,
        out_cont: usize,
        value: &EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        // The predicate's frozen scope ends when its output is consumed:
        // outputs 2..n of a multi-output predicate drive unfrozen. (Holding
        // the freeze across outputs makes the register refuse every
        // publication — `tracks_result` declines while frozen — so the
        // consumer-exit spelling is the one that works today.)
        if !truth::is_truthy(value).map_err(|_| internal_contract("select truth check failed"))? {
            return Ok(None);
        }
        self.path_exit_frozen();

        let emission = {
            let GraphFrame::SelectBody { input, .. } = self.routed_frame(index) else {
                unreachable!("emission walk already classified SelectBody");
            };
            input.try_clone().map_err(EngineRunError::Codec)?
        };
        self.route(emission, out_cont, resources)
    }
}
