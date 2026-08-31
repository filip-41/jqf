//! Remaining builtin evaluators: answer, math, time, regex, process, streams,
//! walk, `map_values`, and the codec-native selector seam.
//!
//! One job: the registry evaluators that are not binary, user-call, keyed,
//! generate, modify, or join. [`super::GraphMachine::eval_call`] is the table
//! that names them.

#[allow(clippy::wildcard_imports)]
use super::super::*;

impl<'program, 'source> GraphMachine<'program, 'source> {
    /// Drives one codec-native selector builtin (`xpath/1`, `css/1`) through
    /// the selector seam.
    ///
    /// The selector text argument follows the ordinary argument-product law:
    /// the call answers once per output of its argument, and an argument that
    /// yields nothing makes the call yield nothing (`xpath(empty)` is empty).
    /// Each activation requires a LOCATED input — the selector runs over the
    /// recovered document authority, never an owned value — whose document
    /// format matches the profile's declared format. The whole computation is
    /// eager (the seam is an owned evaluation over the document), and the
    /// results stream out of a [`GraphFrame::SelectorEmit`] producer frame, so
    /// a failure (a compile rejection, a budget exhaustion, a format
    /// mismatch) is raised only after every already-computed element has been
    /// routed — the same prefix law `PathEmit` keeps.
    #[allow(
        clippy::too_many_lines,
        reason = "one evaluator: locate, evaluate the selector argument, compile, select, emit"
    )]
    pub(crate) fn eval_selector(
        &mut self,
        law: selector_builtins::SelectorLaw,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        let argument = match &self.nodes[node.index()] {
            ProgramNode::Call { args, .. } => *args
                .first()
                .ok_or_else(|| internal_contract("a selector call with no argument"))?,
            _ => return Err(internal_contract("selector dispatch over a non-call node")),
        };
        let located = match &input {
            EngineResult::Located(located) => located.try_clone().map_err(EngineRunError::Codec)?,
            EngineResult::Owned(_) => {
                let text = alloc::format!(
                    "{} requires a recovered {} document as input",
                    law.name(),
                    law.language().format()
                );
                return Err(jqf_builtins::semantics::path::raise(&text, resources));
            }
        };
        let source_node = located.node();
        let root = self.materialize_capture(input, resources)?;
        let slots = self.path_slots(&root, Some(source_node), resources)?;
        let nodes = self.nodes;
        let outputs = path_register::evaluate_owned_filter(
            nodes,
            resources,
            slots,
            argument,
            root.clone(),
            self.topk,
            &mut self.fact_deltas,
        )?;
        let document = located.product().document();
        let mut results: alloc::collections::VecDeque<SelectorEmitValue> = alloc::collections::VecDeque::new();
        let mut pending = None;
        for output in outputs {
            let Value::String(text) = output.untagged() else {
                let kind = message::kind_name(output.kind());
                let text = alloc::format!("{}: the selector argument must be a string, not {}", law.name(), kind);
                pending = Some(jqf_builtins::semantics::path::raise(&text, resources));
                break;
            };
            let compiled = match jqf_builtins::selector::compile(law.language(), text) {
                Ok(compiled) => compiled,
                Err(error) => {
                    let text = selector_builtins::selector_message(law, &error);
                    pending = Some(jqf_builtins::semantics::path::raise(&text, resources));
                    break;
                }
            };
            let budget = jqf_builtins::selector::SelectorBudget::default();
            match jqf_builtins::selector::select(document, source_node, &compiled, budget, resources) {
                Ok(jqf_builtins::selector::SelectorResult::Elements(ids)) => {
                    results.extend(ids.into_iter().map(SelectorEmitValue::Node));
                }
                Ok(jqf_builtins::selector::SelectorResult::Scalar(scalar)) => {
                    let value = match scalar {
                        jqf_builtins::selector::ScalarResult::Number(number) => {
                            // An integral count publishes as an EXACT integer
                            // (jqf's exact-number law — `count(//item)` prints
                            // `2`, never `2.0`); only a genuinely fractional
                            // result stays a binary64.
                            // The range guard makes the casts exact (an
                            // integral f64 within the i64 bounds converts
                            // losslessly); clippy cannot see the guard.
                            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                            {
                                if number.fract() == 0.0 && number >= i64::MIN as f64 && number <= i64::MAX as f64 {
                                    Value::Number(Number::integer(Integer::from_i64(number as i64)))
                                } else {
                                    Value::Number(Number::float(jqf_data::Float::new(number)))
                                }
                            }
                        }
                        jqf_builtins::selector::ScalarResult::Text(text) => {
                            Value::try_string(&text).map_err(|_| EngineRunError::allocation_failure())?
                        }
                    };
                    results.push_back(SelectorEmitValue::Owned(value));
                }
                Err(error) => {
                    let text = selector_builtins::selector_message(law, &error);
                    pending = Some(jqf_builtins::semantics::path::raise(&text, resources));
                    break;
                }
            }
        }
        self.push_frame(GraphFrame::SelectorEmit {
            input: EngineResult::Located(located),
            results,
            pending,
            out_cont: cont,
        });
        Ok(None)
    }

    /// Advances the resumable walk producer by one path: drive the top body
    /// machine, start the next pipeline stage over its item, publish a final
    /// item's register path, or — when every machine is spent — walk to the
    /// next child.
    #[allow(
        clippy::too_many_lines,
        reason = "one resumable step with five distinct outcomes (drive a body machine, \
                  feed a stage, publish, seed, exhaust), each with its own frame access"
    )]
    pub(crate) fn advance_path_walk_emit(
        &mut self,
        index: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, EngineRunError> {
        loop {
            // Pop the top body machine, if one is live, and drive it.
            let machine = match self.frames.as_mut_slice().get_mut(index) {
                Some(GraphFrame::PathWalkEmit { machines, .. }) => machines.pop(),
                _ => {
                    return Err(internal_contract("path walk frame kind changed under advance"));
                }
            };
            if let Some((stage, mut machine)) = machine {
                machine.inherit_fact_deltas(self);
                match machine.advance(resources) {
                    Ok(RunPoll::Item(item)) => {
                        self.adopt_child_fact_deltas(&machine);
                        // The popped machine was pipeline stage `machines.len()`
                        // (the count of stages still below it); its item feeds
                        // the NEXT stage, and an item from the LAST stage is a
                        // final output that publishes the register path. The
                        // machine itself is pushed BACK in both cases — it may
                        // produce more items — exactly as `CallableBody` holds
                        // its child machine between resumptions.
                        let below = match self.frames.as_slice().get(index) {
                            Some(GraphFrame::PathWalkEmit { machines, .. }) => machines.len(),
                            _ => {
                                return Err(internal_contract("path walk frame kind changed under item"));
                            }
                        };
                        let pipeline_len = match self.frames.as_slice().get(index) {
                            Some(GraphFrame::PathWalkEmit { pipeline, .. }) => pipeline.len(),
                            _ => {
                                return Err(internal_contract("path walk frame kind changed under item"));
                            }
                        };
                        if below + 1 < pipeline_len {
                            // Feed the item into the next pipeline stage.
                            let next = match self.frames.as_slice().get(index) {
                                Some(GraphFrame::PathWalkEmit { pipeline, .. }) => pipeline[below + 1],
                                _ => {
                                    return Err(internal_contract("path walk frame kind changed under item"));
                                }
                            };
                            self.push_walk_machine(index, stage, machine)?;
                            self.seed_walk_stage(index, next, item)?;
                            continue;
                        }
                        // A FINAL output: publish the walker's register path.
                        self.push_walk_machine(index, stage, machine)?;
                        let (path, out_cont) = {
                            let frame = self
                                .frames
                                .as_mut_slice()
                                .get_mut(index)
                                .ok_or_else(|| internal_contract("path walk frame vanished under item"))?;
                            let GraphFrame::PathWalkEmit { walk, out_cont, .. } = frame else {
                                return Err(internal_contract("path walk frame kind changed under item"));
                            };
                            (walk.current_path(resources)?, *out_cont)
                        };
                        let result = self.retain_owned(path);
                        self.pending = Some(GraphTask::Route {
                            value: result,
                            cont: out_cont,
                        });
                        return Ok(true);
                    }
                    Ok(RunPoll::Complete) => {
                        self.adopt_child_fact_deltas(&machine);
                        if let Some(GraphFrame::PathWalkEmit { reuse, .. }) = self.frames.as_mut_slice().get_mut(index)
                        {
                            *reuse = Some(Box::new(machine));
                        }
                        continue;
                    }
                    Ok(RunPoll::Pending) => {
                        self.adopt_child_fact_deltas(&machine);
                        // The body machine's slice ran out: push it back at
                        // its stage (exactly as the Item arm does) and let the
                        // enclosing advance loop's next admission surface the
                        // pause — the parent's own admission was consumed
                        // before this task ran.
                        self.push_walk_machine(index, stage, machine)?;
                        return Ok(true);
                    }
                    Err(error) => {
                        self.adopt_child_fact_deltas(&machine);
                        // The body's failure is the CALL's failure: it routes at
                        // the call's own out-continuation, exactly where an
                        // eagerly evaluated body's raise routed. The published
                        // prefix stands.
                        let out_cont = match self.frames.as_slice().get(index) {
                            Some(GraphFrame::PathWalkEmit { out_cont, .. }) => *out_cont,
                            _ => {
                                return Err(internal_contract("path walk frame kind changed under raise"));
                            }
                        };
                        self.raise_cont = out_cont;
                        return Err(error);
                    }
                }
            }
            // No body machine is live: walk to the next child.
            let step = {
                let frame = self
                    .frames
                    .as_mut_slice()
                    .get_mut(index)
                    .ok_or_else(|| internal_contract("path walk frame vanished under walk"))?;
                let GraphFrame::PathWalkEmit {
                    walk,
                    pipeline,
                    include_root,
                    out_cont,
                    ..
                } = frame
                else {
                    return Err(internal_contract("path walk frame kind changed under walk"));
                };
                let mut scratch = StepScratch::new(&mut self.workspace, resources).with_deltas(&self.fact_deltas);
                match walk.next_child(&mut scratch)? {
                    Some(child) => {
                        if pipeline.is_empty() {
                            // `paths/0` and `path(..)`: publish the walked
                            // child's register path directly. The root is
                            // excluded for `paths` by the register's depth.
                            if !*include_root && walk.at_root() {
                                WalkStep::Skip
                            } else {
                                WalkStep::Publish(walk.current_path(scratch.resources())?, *out_cont)
                            }
                        } else {
                            WalkStep::SeedChild(child)
                        }
                    }
                    None => WalkStep::Exhausted,
                }
            };
            match step {
                WalkStep::Skip => {}
                WalkStep::Publish(path, out_cont) => {
                    let result = self.retain_owned(path);
                    self.pending = Some(GraphTask::Route {
                        value: result,
                        cont: out_cont,
                    });
                    return Ok(true);
                }
                WalkStep::SeedChild(child) => {
                    // T1 inline path: identity-preserving LEAF stages over a
                    // walked child are evaluated directly, skipping the
                    // per-child GraphMachine seed/advance/drop (twice per
                    // child — Item then Complete — for every walked child on
                    // the L15 `[paths(scalars)]` lane). A kind admission
                    // answers from the child's own kind tag and emits the
                    // input or nothing; any stage that is not a recognized
                    // leaf declines to the ordinary per-stage machine, and a
                    // stage that declines after an inlined prefix is seeded
                    // exactly where the machine path would have put it.
                    let (pipeline_len, out_cont) = match self.frames.as_slice().get(index) {
                        Some(GraphFrame::PathWalkEmit { pipeline, out_cont, .. }) => (pipeline.len(), *out_cont),
                        _ => {
                            return Err(internal_contract("path walk frame kind changed under seed"));
                        }
                    };
                    let input = child;
                    let mut stage_index = 0usize;
                    loop {
                        let stage = match self.frames.as_slice().get(index) {
                            Some(GraphFrame::PathWalkEmit { stages, pipeline, .. }) => {
                                if stage_index < stages.len() {
                                    stages[stage_index]
                                } else {
                                    WalkStage::Machine(pipeline[stage_index])
                                }
                            }
                            _ => {
                                return Err(internal_contract("path walk frame kind changed under seed"));
                            }
                        };
                        match self.walk_stage_inline(stage, &input, resources) {
                            Err(error) => {
                                // The inline stage's raise is the CALL's
                                // raise: it routes at the call's own
                                // out-continuation, exactly as the machine
                                // arm routes it, and the published prefix
                                // stands.
                                self.raise_cont = out_cont;
                                return Err(error);
                            }
                            // Not a recognized leaf: seed a machine for this
                            // stage over the input and let the enclosing
                            // advance loop drive it.
                            Ok(None) => {
                                let node = match stage {
                                    WalkStage::Machine(node) => node,
                                    WalkStage::Kind { .. } => {
                                        return Err(internal_contract("kind walk stage declined to a machine"));
                                    }
                                };
                                self.seed_walk_stage(index, node, input)?;
                                break;
                            }
                            // An inline verdict: the stage emits its input on
                            // admit, nothing on reject.
                            Ok(Some(admit)) => {
                                if !admit {
                                    // Zero outputs: the child produces no
                                    // path. Drop it and walk to the next
                                    // child.
                                    break;
                                }
                                if stage_index + 1 == pipeline_len {
                                    // A FINAL inline output publishes the
                                    // walker's register path — the Publish
                                    // arm's law, unchanged.
                                    let path = {
                                        let frame = self.frames.as_mut_slice().get_mut(index).ok_or_else(|| {
                                            internal_contract("path walk frame vanished under inline publish")
                                        })?;
                                        let GraphFrame::PathWalkEmit { walk, .. } = frame else {
                                            return Err(internal_contract(
                                                "path walk frame kind changed under inline \
                                                 publish",
                                            ));
                                        };
                                        walk.current_path(resources)?
                                    };
                                    let result = self.retain_owned(path);
                                    self.pending = Some(GraphTask::Route {
                                        value: result,
                                        cont: out_cont,
                                    });
                                    return Ok(true);
                                }
                                stage_index += 1;
                            }
                        }
                    }
                }
                WalkStep::Exhausted => {
                    self.frames.pop();
                    return Ok(true);
                }
            }
        }
    }

    /// Returns one body machine to its pipeline position after a resumption.
    pub(crate) fn push_walk_machine(
        &mut self,
        index: usize,
        node: ProgramNodeId,
        machine: GraphMachine<'program, 'source>,
    ) -> Result<(), EngineRunError> {
        match self.frames.as_mut_slice().get_mut(index) {
            Some(GraphFrame::PathWalkEmit { machines, .. }) => {
                machines
                    .try_reserve(1)
                    .map_err(|_| EngineRunError::allocation_failure())?;
                machines.push((node, machine));
                Ok(())
            }
            _ => Err(internal_contract("path walk frame kind changed under push")),
        }
    }

    /// Seeds one pipeline-stage machine over `input`, writing the frame's
    /// binder-slot snapshot into it.
    pub(crate) fn seed_walk_stage(
        &mut self,
        index: usize,
        node: ProgramNodeId,
        input: EngineResult<'source>,
    ) -> Result<(), EngineRunError> {
        let reused = match self.frames.as_mut_slice().get_mut(index) {
            Some(GraphFrame::PathWalkEmit { reuse, .. }) => reuse.take(),
            _ => return Err(internal_contract("path walk frame kind changed under seed")),
        };
        let mut machine = if let Some(mut machine) = reused {
            machine.reseed(node, input);
            machine
        } else {
            Box::new(self.seed_nested(node, input, true)?)
        };
        // A parent write between two child polls must be visible on resume.
        // Reseed clears the overlay; inherit every seed, reused or not.
        machine.inherit_fact_deltas(self);
        // The snapshot is the same for every child. Write it once; a reused
        // machine already holds it. An empty snapshot is a no-op. The env is
        // sized to the PROGRAM's total slot count — the body's own binders
        // write their occurrences' GLOBAL slot indices, which can lie past
        // the snapshot's bound prefix — exactly as `CallableBody` seeds its
        // child machine with `self.slots` and then writes the overlay.
        let load_env = machine.env.is_empty();
        if load_env {
            let Some(GraphFrame::PathWalkEmit { slots, .. }) = self.frames.as_slice().get(index) else {
                return Err(internal_contract("path walk frame kind changed under seed"));
            };
            if !slots.is_empty() {
                for (slot_index, value) in slots.iter().enumerate() {
                    if let Some(value) = value {
                        let cloned = value.try_clone().map_err(EngineRunError::Codec)?;
                        machine.write_slot(
                            u32::try_from(slot_index).map_err(|_| internal_contract("walk slot index overflow"))?,
                            cloned,
                        )?;
                    }
                }
            }
        }
        match self.frames.as_mut_slice().get_mut(index) {
            Some(GraphFrame::PathWalkEmit { machines, .. }) => {
                machines
                    .try_reserve(1)
                    .map_err(|_| EngineRunError::allocation_failure())?;
                machines.push((node, *machine));
                Ok(())
            }
            _ => Err(internal_contract("path walk frame kind changed under seed")),
        }
    }

    /// Inline evaluation of one identity-preserving pipeline stage over a
    /// walked child, when the stage needs no graph machine.
    ///
    /// Recognizes the leaf shapes the `paths(f)` lowering builds: a bare kind
    /// filter (`scalars`, `objects`, …) and a `select` whose predicate is a
    /// bare kind filter — the `paths(scalars)` / `paths(objects)` spellings.
    ///
    /// The two shapes admit by DIFFERENT laws and the difference is the law,
    /// not an optimization detail:
    ///
    /// - A bare kind filter emits its input when the kind matches. `false`
    ///   and `null` are scalars, so `.. | scalars` emits them.
    /// - `select(f)` emits its input when `f`'s OUTPUT is truthy, and a kind
    ///   filter's output IS the input. So `select(scalars)` admits the kind
    ///   and then fails on the value: `paths(scalars)` omits every `false` and
    ///   `null` scalar, and matching that is the point.
    ///
    /// Truth comes from [`truth::is_truthy`], the one spelling the machine's
    /// own `select` and the SDK's `-e` both read.
    ///
    /// Raises only where `type/0` itself raises — the same raise the machine
    /// path routes at the call's out-continuation.
    ///
    /// Returns `Ok(None)` when the stage is not a recognized leaf (the caller
    /// seeds a full machine for it), `Ok(Some(admit))` for an inline verdict,
    /// and the raise as `Err`.
    #[allow(
        clippy::unused_self,
        reason = "kept as a machine method so the walk advance calls it in the same form as seed_walk_stage"
    )]
    pub(crate) fn walk_stage_inline(
        &self,
        stage: WalkStage,
        input: &EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<bool>, EngineRunError> {
        // The stage was classified when the walk opened. A kind filter is
        // applied here; anything else seeds a machine.
        let WalkStage::Kind { filter, through_select } = stage else {
            return Ok(None);
        };
        let (kind, truthy) =
            truth::kind_and_truthy(input).map_err(|_| internal_contract("walk kind derivation failed"))?;
        let admitted = match filter {
            kind_builtins::KindFilter::Boolean => kind == ValueKind::Bool,
            kind_builtins::KindFilter::Number => kind == ValueKind::Number,
            kind_builtins::KindFilter::String => {
                kind == ValueKind::String || (kind.is_temporal() && resources.types_as_strings())
            }
            kind_builtins::KindFilter::Array => kind == ValueKind::Array,
            kind_builtins::KindFilter::Object => kind == ValueKind::Object,
            kind_builtins::KindFilter::Iterable => {
                matches!(kind, ValueKind::Array | ValueKind::Object)
            }
            kind_builtins::KindFilter::Scalar => !matches!(kind, ValueKind::Array | ValueKind::Object),
        };
        if !admitted {
            return Ok(Some(false));
        }
        if through_select {
            return Ok(Some(truthy));
        }
        Ok(Some(true))
    }

    /// Opens one argument-answering drive: retain the subject, then run the
    /// argument filter into the frame that answers per output.
    ///
    /// The subject is NOT validated here. The argument filter is evaluated
    /// first, so a filter that raises pre-empts the subject's own rejection and a
    /// filter that produces nothing suppresses it entirely: `null | bsearch(1)`
    /// is a rejection while `null | bsearch(empty)` is silence, and
    /// `1 | has(empty)` publishes nothing rather than refusing the number.
    pub(crate) fn eval_answer(
        &mut self,
        form: AnswerForm,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let argument = match &self.nodes[node.index()] {
            ProgramNode::Call { args, .. } => *args
                .first()
                .ok_or_else(|| internal_contract("an answering builtin call with no argument"))?,
            _ => {
                return Err(internal_contract("an answering dispatch over a non-call node"));
            }
        };
        // The argument is a frozen subexpression (the pop-exit releases it).
        self.path_enter_frozen();
        self.raise_cont = cont;
        // The argument filter runs over the SAME input the answer is computed
        // against, so the subject is duplicated before the drive opens.
        let (subject, seed) = match form {
            AnswerForm::HasKey => {
                let seed = input.try_clone().map_err(|_| EngineRunError::allocation_failure())?;
                (AnswerSubject::HasKey(input), seed)
            }
            form => {
                let owned = self.materialize_subject_memoized(input, resources)?;
                let seed = owned.clone();
                let subject = match form {
                    AnswerForm::BinarySearch => AnswerSubject::BinarySearch(owned),
                    AnswerForm::Join => AnswerSubject::Join(owned),
                    AnswerForm::Format => AnswerSubject::Format(owned),
                    AnswerForm::Text(law) => AnswerSubject::Text { law, subject: owned },
                    AnswerForm::Time(law) => AnswerSubject::Time { law, subject: owned },
                    _ => AnswerSubject::Flatten(owned),
                };
                (subject, EngineResult::owned(seed))
            }
        };
        self.push_frame(GraphFrame::ArgumentAnswer {
            subject,
            out_cont: cont,
        });
        self.pending = Some(GraphTask::Eval {
            node: argument,
            input: seed,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Answers one argument output against the retained subject and routes it.
    pub(crate) fn consume_answer(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let target = self.materialize_capture(value, resources)?;
        let (subject, out_cont) = match self.frames.as_slice().get(index) {
            Some(GraphFrame::ArgumentAnswer { subject, out_cont }) => (subject.try_clone()?, *out_cont),
            _ => return Err(internal_contract("answer frame kind changed under capture")),
        };
        self.raise_cont = out_cont;
        let answer = subject.answer(&target, resources)?;
        // `getpath` IS a path expression: inside a register-active body it
        // extends the register by the components its argument names
        // (`path(getpath(["a","b"]))` is the path `["a","b"]`). The answer is
        // the register's value now, so it must NOT be boxed.
        if self.path.is_some() && matches!(&subject, AnswerSubject::GetPath(_)) {
            self.path_follow(&target, &answer, resources)?;
            return self.route(EngineResult::owned(answer), out_cont, resources);
        }
        let result = self.retain_owned(answer);
        self.route(result, out_cont, resources)
    }

    /// Extends the register by every component of one runtime path array
    /// (the `getpath` follow), setting the register's address to the answered
    /// value.
    pub(crate) fn path_follow(
        &mut self,
        path: &Value,
        answer: &Value,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        // A frozen subexpression computes VALUES, never register extensions:
        // every navigation entry point declines the same way while frozen
        // (`path_check_navigation`, `path_each_child`), so a `getpath` inside
        // one contributes no steps and its answered value still routes below.
        if self.path_frozen() {
            return Ok(());
        }
        let steps = path_register::follow_steps(path, resources)?;
        let answer_owned = answer.clone();
        let (steps_len, at, at_node) = {
            let Some(register) = self.path.as_mut() else {
                return Err(internal_contract("path_follow without its register"));
            };
            let pre = register.pre_navigation();
            for step in steps {
                register.push_step(step, answer_owned.clone(), None)?;
            }
            pre
        };
        self.push_frame(GraphFrame::PathStep { steps_len, at, at_node });
        Ok(())
    }

    /// Dispatches one math evaluator over its call node.
    ///
    /// The /0 forms are owned-value laws (or input-ignoring constants, or the
    /// never-raising check quartet); the /2 and /3 forms open the argument
    /// nesting and return, exactly like `eval_binary` returns after seeding
    /// its right operand.
    pub(crate) fn eval_math(
        &mut self,
        evaluator: math_builtins::MathEvaluator,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        match evaluator {
            math_builtins::MathEvaluator::Unary(law) => {
                self.eval_owned_law(input, cont, resources, move |subject, resources| {
                    math_builtins::apply_unary(law, subject, resources)
                })
            }
            math_builtins::MathEvaluator::Trig(law) => {
                self.eval_owned_law(input, cont, resources, move |subject, resources| {
                    math_builtins::apply_trig(law, subject, resources)
                })
            }
            math_builtins::MathEvaluator::Gamma(law) => {
                self.eval_owned_law(input, cont, resources, move |subject, resources| {
                    math_builtins::apply_gamma(law, subject, resources)
                })
            }
            math_builtins::MathEvaluator::Special(law) => {
                self.eval_owned_law(input, cont, resources, move |subject, resources| {
                    math_builtins::apply_special(law, subject, resources)
                })
            }
            math_builtins::MathEvaluator::Check(law) => {
                self.eval_owned_law(input, cont, resources, move |subject, _| {
                    Ok(math_builtins::apply_check(law, subject))
                })
            }
            // `nan` and `infinite` publish their value regardless of the input
            // (`"x" | infinite` is `1.7976931348623157e+308`), so there is no
            // materialization: the piped handle is dropped and the fresh owned
            // value is routed.
            math_builtins::MathEvaluator::Const(law) => {
                let value = math_builtins::apply_const(law);
                let result = self.retain_owned(value);
                self.route(result, cont, resources)
            }
            math_builtins::MathEvaluator::Bin(law) => self.eval_math_args(MathCallLaw::Bin(law), node, input, cont),
            math_builtins::MathEvaluator::Tern(law) => self.eval_math_args(MathCallLaw::Tern(law), node, input, cont),
        }
    }

    /// Dispatches one date/time evaluator over its call node.
    ///
    /// The /0 forms are owned-value laws (or `now`, which ignores the piped
    /// value and publishes the wall clock, exactly like the math constants);
    /// the /1 format laws open the argument-answer drive and return, so each
    /// format output is answered independently.
    pub(crate) fn eval_time(
        &mut self,
        evaluator: TimeEvaluator,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        match evaluator {
            TimeEvaluator::Unary(law) => match law {
                // `now` ignores the piped value: no materialization, the fresh
                // owned value is routed and the handle dropped.
                TimeLaw::Now => {
                    let value = time_builtins::apply_unary(TimeLaw::Now, &Value::Null, resources)?;
                    let result = self.retain_owned(value);
                    self.route(result, cont, resources)
                }
                law => self.eval_owned_law(input, cont, resources, move |subject, resources| {
                    time_builtins::apply_unary(law, subject, resources)
                }),
            },
            TimeEvaluator::Format(law) => self.eval_answer(AnswerForm::Time(law), node, input, cont, resources),
        }
    }

    /// Dispatches one regex evaluator over its call node.
    ///
    /// The argument filters are evaluated EAGERLY over the materialized input
    /// (the same nested-machine drive the path-family argument laws use), and
    /// the computed outputs stream out of a producer frame. `sub`/`gsub` are
    /// two-phase: the pattern+flags combination yields the match set, the
    /// replacement filter runs once per match with dot = the capture object,
    /// and the assembled strings publish together.
    pub(crate) fn eval_regex(
        &mut self,
        evaluator: regex_builtins::RegexLaw,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        let law = evaluator;
        let args = match &self.nodes[node.index()] {
            ProgramNode::Call { args, .. } => args.clone(),
            _ => return Err(internal_contract("regex dispatch over a non-call node")),
        };
        // The match-ITERATOR family (`match`/`capture`/`scan`) inside a real
        // register drive: its EACH over the match array always fails the
        // register check, so the iterate error raises even when nothing
        // matches.
        if self.path.is_some()
            && matches!(
                law,
                regex_builtins::RegexLaw::Match1
                    | regex_builtins::RegexLaw::Match2
                    | regex_builtins::RegexLaw::Capture1
                    | regex_builtins::RegexLaw::Capture2
                    | regex_builtins::RegexLaw::Scan1
                    | regex_builtins::RegexLaw::Scan2
            )
        {
            let subject = self.materialize_capture(input, resources)?;
            let mut arg = |node: ProgramNodeId| -> Result<Value, EngineRunError> {
                path_register::evaluate_owned_filter(
                    self.nodes,
                    resources,
                    Vec::new(),
                    node,
                    subject.clone(),
                    self.topk,
                    &mut self.fact_deltas,
                )
                .map(|mut values| values.drain(..).next().unwrap_or(Value::Null))
            };
            let pattern = arg(args
                .first()
                .copied()
                .ok_or_else(|| internal_contract("regex match call with no pattern"))?)?;
            let flags = args.get(1).map(|node| arg(*node)).transpose()?.unwrap_or(Value::Null);
            let matches = regex_builtins::apply_combo(law, &subject, &pattern, &flags, resources)?;
            let array = jqf_data::Array::try_from_vec(matches)
                .map(Value::Array)
                .map_err(|_| EngineRunError::allocation_failure())?;
            return Err(jqf_builtins::semantics::path::invalid_iterate(&array, resources));
        }
        let source_node = input.located().map(LocatedProduct::node);
        let root = self.materialize_capture(input, resources)?;
        let slots = self.path_slots(&root, source_node, resources)?;
        let nodes = self.nodes;
        let (values, pending) = regex_drive(
            nodes,
            resources,
            &root,
            &slots,
            law,
            &args,
            self.topk,
            &mut self.fact_deltas,
        )?;
        if values.is_empty() && pending.is_none() {
            return Ok(None);
        }
        self.push_frame(GraphFrame::PathEmit {
            values,
            cursor: 0,
            // The drive's argument error after some combinations were
            // computed: the prefix is published, then the error raises —
            // the published-prefix law.
            pending,
            out_cont: cont,
        });
        Ok(None)
    }

    /// Writes one `debug` message line to the host's stderr channel.
    ///
    /// The law: `["DEBUG:",<compact value>]` followed by a NEWLINE, and the
    /// piped value published unchanged. (`stderr/0` writes
    /// the bare compact value with NO newline, which is why the two cannot
    /// share a sink call.) The line is assembled from the rendered value rather
    /// than from a constructed two-element array, so a message costs no `Value`
    /// allocation at all.
    ///
    /// Without a host channel the pass-through law still holds: the message is
    /// a diagnostic, never part of the program's value.
    pub(crate) fn write_debug_message(value: &Value, resources: &ResourceContext<'_>) -> Result<(), EngineRunError> {
        const OPENING: &str = "[\"DEBUG:\",";
        const CLOSING: &str = "]\n";
        let Some(sink) = resources.stderr_sink() else {
            return Ok(());
        };
        let mut line = String::new();
        line.try_reserve(OPENING.len() + CLOSING.len())
            .map_err(|_| EngineRunError::allocation_failure())?;
        line.push_str(OPENING);
        jqf_builtins::semantics::render::write_value(&mut line, value)?;
        line.push_str(CLOSING);
        sink.write_compact(line.as_bytes()).map_err(|error| {
            EngineRunError::Codec(jqf_codec_core::CodecError::new(
                jqf_codec_core::CodecFailureKind::Resource(error),
            ))
        })
    }

    /// Dispatches one misc-rider evaluator over its call node.
    ///
    /// `builtins` and `have_decnum` ignore the piped value and publish owned
    /// values (like `now`); `debug/0` writes its message and routes the piped
    /// value through, and `debug/1` writes one message per output of its
    /// argument and then routes the input through.
    pub(crate) fn eval_rider(
        &mut self,
        evaluator: rider_builtins::RiderEvaluator,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        match evaluator {
            rider_builtins::RiderEvaluator::Unary(law) => match law {
                rider_builtins::RiderLaw::Builtins => {
                    let value = rider_builtins::builtins(resources)?;
                    let result = self.retain_owned(value);
                    self.route(result, cont, resources)
                }
                // The two capability probes currently answer the same way; the
                // arms are merged because their bodies are identical.
                rider_builtins::RiderLaw::HaveDecnum | rider_builtins::RiderLaw::HaveLiteralNumbers => {
                    let value = rider_builtins::have_decnum();
                    let result = self.retain_owned(value);
                    self.route(result, cont, resources)
                }
                // `finites`/`normals` are `select(<f64 predicate>)` filters: a
                // number the predicate admits routes through UNCHANGED (the
                // original handle, like the kind filters), anything else emits
                // nothing. The predicate reads a BORROWED view of a located
                // number, so the handle never materializes.
                law @ (rider_builtins::RiderLaw::Finites | rider_builtins::RiderLaw::Normals) => {
                    if rider_builtins::result_number_passes(&input, law)? {
                        self.route(input, cont, resources)
                    } else {
                        Ok(None)
                    }
                }
                // `debug/0`: write the message, then route the piped value
                // through. Only the MESSAGE needs an owned value, so the run
                // materializes a copy and the original handle still travels
                // zero-copy, exactly like the kind filters.
                rider_builtins::RiderLaw::Debug => {
                    if resources.stderr_sink().is_some() {
                        let value = self.materialize_capture(
                            input.try_clone().map_err(|_| EngineRunError::allocation_failure())?,
                            resources,
                        )?;
                        Self::write_debug_message(&value, resources)?;
                    }
                    self.route(input, cont, resources)
                }
            },
            rider_builtins::RiderEvaluator::DebugOne => {
                // `debug(msgs)` is defined as `(msgs | debug | empty), .`, so
                // the argument is an ordinary filter over the same input and
                // ITERATES: one message line per output, an EMPTY argument
                // writes nothing and still publishes the input, and an argument
                // that raises surfaces here rather than being swallowed.
                let argument = match &self.nodes[node.index()] {
                    ProgramNode::Call { args, .. } => *args
                        .first()
                        .ok_or_else(|| internal_contract("debug/1 call with no argument"))?,
                    _ => return Err(internal_contract("debug dispatch over a non-call node")),
                };
                let source_node = input.located().map(LocatedProduct::node);
                let root = self.materialize_capture(input, resources)?;
                let slots = self.path_slots(&root, source_node, resources)?;
                let nodes = self.nodes;
                let seed = root.clone();
                let messages = path_register::evaluate_owned_filter(
                    nodes,
                    resources,
                    slots,
                    argument,
                    seed,
                    self.topk,
                    &mut self.fact_deltas,
                )?;
                for message in &messages {
                    Self::write_debug_message(message, resources)?;
                }
                self.route(EngineResult::owned(root), cont, resources)
            }
        }
    }

    /// Dispatches one host-state/process evaluator over its call node.
    ///
    /// All four current laws ignore the piped value and publish one owned value
    /// from the request's environment snapshot (`env`'s object, the two origin
    /// strings, and the search list). Without a snapshot the empty forms answer:
    /// `env` → `{}`, `get_prog_origin` → `null`, `get_jq_origin` → `"."`, and
    /// `get_search_list` → the literal default list.
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per process builtin: the evaluator IS the table, and splitting it                   would hide which evaluators are covered"
    )]
    pub(crate) fn eval_process(
        &mut self,
        evaluator: process_builtins::ProcessEvaluator,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        match evaluator {
            // `halt_error/1` evaluates its status argument over the current
            // input (the ordinary filter-argument law), then terminates the
            // run with the input as the message. An EMPTY argument means the
            // call never fires (`halt_error(empty)` halts nothing), and a
            // non-number argument raises the dedicated refusal.
            process_builtins::ProcessEvaluator::HaltErrorOne => {
                let argument = match &self.nodes[node.index()] {
                    ProgramNode::Call { args, .. } => *args
                        .first()
                        .ok_or_else(|| EngineRunError::internal_contract("halt_error/1 call with no argument"))?,
                    _ => {
                        return Err(EngineRunError::internal_contract(
                            "halt_error dispatch over a non-call node",
                        ));
                    }
                };
                let source_node = input.located().map(LocatedProduct::node);
                let message_value = self.materialize_capture(
                    input.try_clone().map_err(|_| EngineRunError::allocation_failure())?,
                    resources,
                )?;
                let root = self.materialize_capture(input, resources)?;
                let slots = self.path_slots(&root, source_node, resources)?;
                let nodes = self.nodes;
                let seed = root.clone();
                let outputs = path_register::evaluate_owned_filter(
                    nodes,
                    resources,
                    slots,
                    argument,
                    seed,
                    self.topk,
                    &mut self.fact_deltas,
                )?;
                let Some(status_value) = outputs.into_iter().next() else {
                    // `halt_error(empty)` never fires, exactly like `error(empty)`.
                    return Ok(None);
                };
                let Value::Number(number) = status_value.untagged() else {
                    // The ARGUMENT is checked but the INPUT is named in the
                    // refusal.
                    let text = process_builtins::halt_error_number_required(&message_value)?;
                    return Err(semantics_path::raise(&text, resources));
                };
                let status = process_builtins::halt_status(jqf_builtins::semantics::order::to_f64(number));
                Err(EngineRunError::Halt {
                    status,
                    message: Some(message_value),
                })
            }
            process_builtins::ProcessEvaluator::Unary(law) => {
                let environment = resources.environment();
                match law {
                    process_builtins::ProcessLaw::Env => {
                        let value = process_builtins::env(environment, resources)?;
                        let result = self.retain_owned(value);
                        self.route(result, cont, resources)
                    }
                    process_builtins::ProcessLaw::GetProgOrigin => {
                        let value = process_builtins::get_prog_origin(environment, resources)?;
                        let result = self.retain_owned(value);
                        self.route(result, cont, resources)
                    }
                    process_builtins::ProcessLaw::GetJqOrigin => {
                        let value = process_builtins::get_jq_origin(environment, resources)?;
                        let result = self.retain_owned(value);
                        self.route(result, cont, resources)
                    }
                    process_builtins::ProcessLaw::GetSearchList => {
                        let value = process_builtins::get_search_list(environment, resources)?;
                        let result = self.retain_owned(value);
                        self.route(result, cont, resources)
                    }
                    // `stderr`: render the input compact, hand the bytes to the
                    // host stderr channel, and route the ORIGINAL handle through
                    // — the callback writes to the host channel and returns the
                    // input. Without a host channel the value law still holds.
                    process_builtins::ProcessLaw::Stderr => {
                        let value = self.materialize_capture(
                            input.try_clone().map_err(|_| EngineRunError::allocation_failure())?,
                            resources,
                        )?;
                        let text = jqf_builtins::semantics::render::to_json(&value)?;
                        if let Some(sink) = resources.stderr_sink() {
                            sink.write_compact(text.as_bytes()).map_err(|error| {
                                EngineRunError::Codec(jqf_codec_core::CodecError::new(
                                    jqf_codec_core::CodecFailureKind::Resource(error),
                                ))
                            })?;
                        }
                        self.route(input, cont, resources)
                    }
                    // `halt`/`halt_error`: process-level termination, never
                    // catchable and never suppressed by `?`.
                    process_builtins::ProcessLaw::Halt => Err(EngineRunError::Halt {
                        status: 0,
                        message: None,
                    }),
                    process_builtins::ProcessLaw::HaltErrorZero => {
                        let value = self.materialize_capture(input, resources)?;
                        Err(EngineRunError::Halt {
                            status: 5,
                            message: Some(value),
                        })
                    }
                    // `input/0`: pull the next value from the shared sequence,
                    // or raise the `break` error (a catch-eligible STRING) at
                    // the end — `try input catch .` answers `"break"`. A parse
                    // refusal from the `--stream` event cursor raises the
                    // message text, catch-eligible (`try input catch .` over a
                    // malformed stream answers the catch).
                    process_builtins::ProcessLaw::Input => {
                        let pull = with_input_source(resources, |source, resources| {
                            let pulled = source.next(resources);
                            if matches!(pulled, Ok(Some(_))) {
                                source.mark_current();
                            }
                            pulled
                        });
                        let value = match pull {
                            Some(Ok(value)) => value,
                            Some(Err(InputSourceError::Refused(message))) => {
                                return Err(EngineRunError::Raised(
                                    Value::try_string(&message).map_err(|_| EngineRunError::allocation_failure())?,
                                ));
                            }
                            Some(Err(InputSourceError::Allocation)) => {
                                return Err(EngineRunError::allocation_failure());
                            }
                            None => None,
                        };
                        match value {
                            Some(value) => {
                                // The PULLED value is marked as the current
                                // input, so `input_filename`/`input_line_number`
                                // evaluated after `input` describe the value it
                                // returned, not the driver's (`-n 'input |
                                // input_line_number'` over `1\n2\n` answers
                                // `2`, the pulled value's line).
                                let result = self.retain_owned(value);
                                self.route(result, cont, resources)
                            }
                            None => Err(EngineRunError::Raised(
                                Value::try_string("break").map_err(|_| EngineRunError::allocation_failure())?,
                            )),
                        }
                    }
                    // `inputs/0`: stream the remaining values from the LAZY
                    // producer frame (the `try repeat(input) catch … then
                    // empty` shape), one pull per resumption — a drain here
                    // read the whole remaining stream before the first value
                    // routed, which no countdown cut could truncate. A
                    // pull-time parse refusal raises from the frame, exactly
                    // as `inputs` raises the parser error (`try [inputs] catch
                    // .` answers the catch).
                    process_builtins::ProcessLaw::Inputs => {
                        self.push_frame(GraphFrame::InputsEmit { out_cont: cont });
                        Ok(None)
                    }
                    process_builtins::ProcessLaw::InputFilename => {
                        let value = match input_source(resources).and_then(|s| s.current_filename()) {
                            Some(name) => Value::try_string(name).map_err(|_| EngineRunError::allocation_failure())?,
                            None => Value::Null,
                        };
                        let result = self.retain_owned(value);
                        self.route(result, cont, resources)
                    }
                    process_builtins::ProcessLaw::InputLineNumber => {
                        let line = input_source(resources).map_or(0, InputSource::current_line);
                        let value = Value::Number(Number::integer(Integer::from_i64(
                            i64::try_from(line).unwrap_or(i64::MAX),
                        )));
                        let result = self.retain_owned(value);
                        self.route(result, cont, resources)
                    }
                    process_builtins::ProcessLaw::Modulemeta => {
                        let value = self.materialize_capture(input, resources)?;
                        let answer = process_builtins::modulemeta(&value, resources)?;
                        let result = self.retain_owned(answer);
                        self.route(result, cont, resources)
                    }
                }
            }
        }
    }

    /// Dispatches one streaming-utility evaluator over its call node.
    ///
    /// `tostream` materializes the input once and opens the resumable
    /// post-order walk (children first — the
    /// `path(def r: (.[]?|r), .; r)` enumeration) as a
    /// [`GraphFrame::TostreamEmit`] producer: one compact pair/marker item per
    /// resumption, never the collected stream. `fromstream` runs its argument
    /// over the input and feeds each event through the `{x,e}` state machine,
    /// emitting each completed root; `truncate_stream` reads its depth
    /// from the piped input, runs its argument with dot = `null` (the
    /// `. as $n | null | stream` shape), and shortens or drops each item.
    ///
    /// Both arguments are driven AS A STREAM — one event per sink call, never
    /// collected first, because `foreach` emits each completed root before
    /// the next event is evaluated: an event that raises must surface
    /// AFTER the earlier roots, which is exactly the prefix law a
    /// [`GraphFrame::PathEmit`] frame's `pending` slot keeps. The frozen
    /// register is the argument-evaluation law's, unchanged.
    #[allow(
        clippy::too_many_lines,
        reason = "the streams dispatch matches per law; the allocation collapse re-wrapped statements across extra lines without adding statements"
    )]
    pub(crate) fn eval_streams(
        &mut self,
        law: streams_builtins::StreamsLaw,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        let argument = |machine: &mut Self| -> Result<ProgramNodeId, EngineRunError> {
            Ok(match &machine.nodes[node.index()] {
                ProgramNode::Call { args, .. } => *args
                    .first()
                    .ok_or_else(|| EngineRunError::internal_contract("streams/1 call with no argument"))?,
                _ => {
                    return Err(EngineRunError::internal_contract(
                        "streams dispatch over a non-call node",
                    ));
                }
            })
        };
        match law {
            streams_builtins::StreamsLaw::Tostream => {
                let value = self.materialize_capture(input, resources)?;
                // The walk is a PRODUCER FRAME, not a collected vector: one
                // emission per resumption keeps a wide document's stream at
                // O(one item) of residency. Every input format's document
                // walks identically (the walk is owned-value machinery with
                // no grammar of its own), so there is no materializing
                // fallback anywhere — the eager collector this replaces was
                // the only shape that held the whole stream at once.
                let walk = streams_builtins::TostreamWalk::try_new(&value, resources)?;
                self.push_frame(GraphFrame::TostreamEmit { walk, out_cont: cont });
            }
            streams_builtins::StreamsLaw::Fromstream => {
                let argument = argument(self)?;
                let source_node = input.located().map(LocatedProduct::node);
                let root = self.materialize_capture(input, resources)?;
                let slots = self.path_slots(&root, source_node, resources)?;
                let nodes = self.nodes;
                let (items, outcome) = path_register::evaluate_owned_filter_prefix(
                    nodes,
                    resources,
                    slots,
                    argument,
                    root.clone(),
                    self.topk,
                    &mut self.fact_deltas,
                )?;
                let mut values = Vec::new();
                let pending = outcome;
                let mut state = streams_builtins::FromstreamState::new();
                for item in items {
                    if let Some(completed) = state.step(&item, resources)? {
                        values
                            .try_reserve(1)
                            .map_err(|_| EngineRunError::allocation_failure())?;
                        values.push(completed);
                    }
                }
                self.push_frame(GraphFrame::PathEmit {
                    values,
                    cursor: 0,
                    pending,
                    out_cont: cont,
                });
            }
            streams_builtins::StreamsLaw::TruncateStream => {
                let argument = argument(self)?;
                let source_node = input.located().map(LocatedProduct::node);
                let depth_value = self.materialize_capture(input, resources)?;
                // The `. as $n | null | stream` shape: the argument runs with
                // dot = `null`; the depth stays the AUTHORED value, because the
                // `(.[0]|length) > $n` comparison is the ORDER law and the
                // slice's start bound is the authored value.
                let slots = self.path_slots(&depth_value, source_node, resources)?;
                let nodes = self.nodes;
                let (items, outcome) = path_register::evaluate_owned_filter_prefix(
                    nodes,
                    resources,
                    slots,
                    argument,
                    Value::Null,
                    self.topk,
                    &mut self.fact_deltas,
                )?;
                let mut values = Vec::new();
                let pending = outcome;
                for item in items {
                    if let Some(kept) = streams_builtins::truncate_item(&depth_value, &item, resources)? {
                        values
                            .try_reserve(1)
                            .map_err(|_| EngineRunError::allocation_failure())?;
                        values.push(kept);
                    }
                }
                self.push_frame(GraphFrame::PathEmit {
                    values,
                    cursor: 0,
                    pending,
                    out_cont: cont,
                });
            }
        }
        Ok(None)
    }

    /// The ONE argument law every jqf-extension evaluator follows.
    ///
    /// These families take ordinary filter arguments, so every argument
    /// ITERATES: `apply` runs once per COMBINATION of the arguments' outputs,
    /// and the drive order is the RIGHT-OUTER Cartesian order — the RIGHTMOST
    /// argument is the OUTER loop, so the leftmost argument advances fastest.
    /// That is the law [`Self::eval_math_args`] drives lazily for the arity-2
    /// math builtins (`[pow(1,2;3,4)]` is `[1,8,1,16]`, pinned against the
    /// reference)
    /// and the law [`Self::eval_pointer`] follows, so every argument-taking
    /// builtin in the engine answers in ONE order.
    ///
    /// An argument with NO outputs makes the product empty, so the call
    /// publishes NOTHING: `log(empty)` is `empty`. It used to raise an
    /// `InternalContractViolation`, and an internal contract must be
    /// unreachable from user input.
    ///
    /// Every value leaves through a [`GraphFrame::PathEmit`] producer, so a law
    /// that raises on a later combination keeps the values already collected
    /// and raises after them — the ordered-prefix law `json_pointer` follows.
    ///
    /// The families differ ONLY in the pure law they apply to one combination,
    /// which is all `apply` is; nothing about the ARGUMENT law is theirs to
    /// restate. Six near-copies of this loop, each wrong in the same way, is
    /// how the defect shipped past a full green example battery.
    pub(crate) fn emit_argument_product<F>(
        &mut self,
        args: &[ProgramNodeId],
        root: &Value,
        slots: &[Option<Value>],
        cont: usize,
        resources: &mut ResourceContext<'_>,
        mut apply: F,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError>
    where
        F: FnMut(Vec<Value>, &mut ResourceContext<'_>) -> Result<Value, EngineRunError>,
    {
        let nodes = self.nodes;
        let mut evaluated = Vec::new();
        evaluated
            .try_reserve_exact(args.len())
            .map_err(|_| EngineRunError::allocation_failure())?;
        for &argument in args {
            let overlay = clone_slots(slots);
            let seed = root.clone();
            evaluated.push(path_register::evaluate_owned_filter(
                nodes,
                resources,
                overlay,
                argument,
                seed,
                self.topk,
                &mut self.fact_deltas,
            )?);
        }
        // An argument-FREE call still has one combination — the empty one — so
        // the laws that read only the input share this path unchanged.
        // The ONE argument law: the shared right-outer product
        // iteration, applied per combination. The law's ORDER lives in
        // [`for_each_argument_combination`] and nowhere else — the same
        // function path mode's builtin dispatch and the callable tail
        // trampoline call, so it cannot diverge between drives.
        let mut values = Vec::new();
        values
            .try_reserve_exact(
                evaluated
                    .iter()
                    .try_fold(1usize, |count, outputs| count.checked_mul(outputs.len()))
                    .ok_or_else(EngineRunError::allocation_failure)?,
            )
            .map_err(|_| EngineRunError::allocation_failure())?;
        let pending = for_each_argument_combination(&mut evaluated, |combination| {
            let value = apply(core::mem::take(combination), resources)?;
            values
                .try_reserve(1)
                .map_err(|_| EngineRunError::allocation_failure())?;
            values.push(value);
            Ok(())
        })?;
        self.push_frame(GraphFrame::PathEmit {
            values,
            cursor: 0,
            pending,
            out_cont: cont,
        });
        Ok(None)
    }

    /// Drives one PARSERS-family law over the piped value.
    ///
    /// The unary laws read only the input; `parse_grok` evaluates its pattern
    /// argument filter over the same input and applies the law once per
    /// pattern, under the shared [`Self::emit_argument_product`] law. A
    /// non-string argument raises the law's catch-eligible error.
    pub(crate) fn eval_parser(
        &mut self,
        law: parser_builtins::ParseLaw,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        let ProgramNode::Call { args, .. } = &self.nodes[node.index()] else {
            return Err(internal_contract("parser dispatch over a non-call node"));
        };
        let source_node = input.located().map(LocatedProduct::node);
        let root = self.materialize_capture(input, resources)?;
        let slots = self.path_slots(&root, source_node, resources)?;
        self.emit_argument_product(args, &root, &slots, cont, resources, |arguments, resources| {
            parser_builtins::parse_law(law, &root, arguments.first(), resources)
        })
    }

    /// Drives one JSON-Pointer law.
    ///
    /// Both arities take the PATH argument as an ordinary filter argument:
    /// EVERY output is one pointer, and each is parsed and navigated in turn.
    /// The SOURCE stream is the input value itself at arity 1 and the first
    /// argument's outputs at arity 2, and PATH — the RIGHTMOST argument — is
    /// the OUTER loop of the right-outer Cartesian argument law (the same law
    /// [`Self::eval_math_args`] and [`Self::eval_binary`] follow), so every
    /// source is navigated by one pointer before the next pointer starts.
    ///
    /// Every array leaves through a [`GraphFrame::PathEmit`] frame, so an error
    /// on a later source or a later pointer keeps the arrays already emitted
    /// and raises after them (the ordered prefix law the historical reference
    /// pins).
    pub(crate) fn eval_pointer(
        &mut self,
        law: pointer_builtins::PointerLaw,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        let ProgramNode::Call { args, .. } = &self.nodes[node.index()] else {
            return Err(internal_contract("pointer dispatch over a non-call node"));
        };
        let source_node = input.located().map(LocatedProduct::node);
        // Both pointer laws are READ-ONLY navigations of the input, so the
        // materialized root is memoized by the located node: a pointer loop
        // over one document otherwise re-materializes the whole document per
        // call — the `pointer_lookup_loop` sibling of the Family 1 law.
        let root = self.materialize_subject_memoized(input, resources)?;
        let slots = self.path_slots(&root, source_node, resources)?;
        let nodes = self.nodes;
        let path_arg = *args
            .last()
            .ok_or_else(|| internal_contract("pointer call with no path argument"))?;
        let overlay = clone_slots(&slots);
        let seed = root.clone();
        let paths = path_register::evaluate_owned_filter(
            nodes,
            resources,
            overlay,
            path_arg,
            seed,
            self.topk,
            &mut self.fact_deltas,
        )?;
        let evaluated_sources = match law {
            pointer_builtins::PointerLaw::Read => None,
            pointer_builtins::PointerLaw::ReadSource => {
                let source_arg = *args
                    .first()
                    .ok_or_else(|| internal_contract("pointer source call with no source argument"))?;
                let overlay = clone_slots(&slots);
                let seed = root.clone();
                Some(path_register::evaluate_owned_filter(
                    nodes,
                    resources,
                    overlay,
                    source_arg,
                    seed,
                    self.topk,
                    &mut self.fact_deltas,
                )?)
            }
        };
        // `json_pointer/1` navigates the input itself, which is already owned
        // here — a one-element borrow of it, never a clone.
        let sources: &[Value] = evaluated_sources.as_deref().unwrap_or(core::slice::from_ref(&root));
        let (values, pending) = navigate_parsed_paths(
            &paths,
            sources,
            resources,
            |path, resources| pointer_builtins::parse_tokens(path, resources),
            |source, tokens, resources| pointer_builtins::match_at(source, tokens, resources),
        )?;
        self.push_frame(GraphFrame::PathEmit {
            values,
            cursor: 0,
            pending,
            out_cont: cont,
        });
        Ok(None)
    }

    #[cfg(feature = "ext-jsonpath")]
    /// Drives one `JSONPath` law.
    ///
    /// Both arities take the QUERY argument as an ordinary filter argument:
    /// EVERY output is one query string, and each is parsed and evaluated in
    /// turn. The SOURCE stream is the input value itself at arity 1 and the
    /// first argument's outputs at arity 2, and QUERY — the RIGHTMOST argument
    /// — is the OUTER loop of the right-outer Cartesian argument law, so
    /// every source is navigated by one query before the next query starts.
    ///
    /// Every nodelist array leaves through a [`GraphFrame::PathEmit`] frame,
    /// so an error on a later source or a later query keeps the arrays
    /// already emitted and raises after them (the ordered prefix law
    /// `json_pointer` pins).
    pub(crate) fn eval_jsonpath(
        &mut self,
        law: jsonpath_builtins::JsonPathLaw,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        let ProgramNode::Call { args, .. } = &self.nodes[node.index()] else {
            return Err(internal_contract("jsonpath dispatch over a non-call node"));
        };
        let source_node = input.located().map(LocatedProduct::node);
        // Both jsonpath laws are READ-ONLY navigations of the input, so the
        // materialized root is memoized by the located node, exactly as
        // `eval_pointer` does.
        let root = self.materialize_subject_memoized(input, resources)?;
        let slots = self.path_slots(&root, source_node, resources)?;
        let nodes = self.nodes;
        let query_arg = *args
            .last()
            .ok_or_else(|| internal_contract("jsonpath call with no query argument"))?;
        let overlay = clone_slots(&slots);
        let seed = root.clone();
        let queries = path_register::evaluate_owned_filter(
            nodes,
            resources,
            overlay,
            query_arg,
            seed,
            self.topk,
            &mut self.fact_deltas,
        )?;
        let evaluated_sources = match law {
            jsonpath_builtins::JsonPathLaw::Read => None,
            jsonpath_builtins::JsonPathLaw::ReadSource => {
                let source_arg = *args
                    .first()
                    .ok_or_else(|| internal_contract("jsonpath source call with no source argument"))?;
                let overlay = clone_slots(&slots);
                let seed = root.clone();
                Some(path_register::evaluate_owned_filter(
                    nodes,
                    resources,
                    overlay,
                    source_arg,
                    seed,
                    self.topk,
                    &mut self.fact_deltas,
                )?)
            }
        };
        // `jsonpath/1` navigates the input itself, which is already owned
        // here — a one-element borrow of it, never a clone.
        let sources: &[Value] = evaluated_sources.as_deref().unwrap_or(core::slice::from_ref(&root));
        let (values, pending) = navigate_parsed_paths(
            &queries,
            sources,
            resources,
            |query, resources| jsonpath_builtins::parse_query(query, resources),
            |source, parsed, resources| jsonpath_builtins::match_query(parsed, source, resources),
        )?;
        self.push_frame(GraphFrame::PathEmit {
            values,
            cursor: 0,
            pending,
            out_cont: cont,
        });
        Ok(None)
    }
    #[cfg(feature = "ext-schema")]
    /// Runs one schema-family law over its VALUE and SCHEMA arguments.
    ///
    /// `schema_infer/1` infers one JSON Schema 2020-12 document from its VALUE
    /// argument. The validation laws run the shared walker over the VALUE and
    /// SCHEMA arguments: `schema_validate/2` routes `true`/`false`,
    /// `schema_errors/2` routes the ordered error-object array. All three take
    /// ordinary filter arguments under the shared
    /// [`Self::emit_argument_product`] law.
    pub(crate) fn eval_schema(
        &mut self,
        law: schema_builtins::SchemaLaw,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        let ProgramNode::Call { args, .. } = &self.nodes[node.index()] else {
            return Err(internal_contract("schema dispatch over a non-call node"));
        };
        let source_node = input.located().map(LocatedProduct::node);
        let root = self.materialize_capture(input, resources)?;
        let slots = self.path_slots(&root, source_node, resources)?;
        self.emit_argument_product(args, &root, &slots, cont, resources, |arguments, resources| match law {
            schema_builtins::SchemaLaw::Infer => {
                let [value] = arguments.as_slice() else {
                    return Err(internal_contract("schema_infer call without one argument"));
                };
                schema_builtins::infer_schema(value, resources)
            }
            schema_builtins::SchemaLaw::InferWithOptions => {
                let [value, options] = arguments.as_slice() else {
                    return Err(internal_contract("schema_infer call without two arguments"));
                };
                let options = schema_builtins::InferOptions::parse(options, resources)?;
                schema_builtins::infer_schema_with_options(value, options, resources)
            }
            schema_builtins::SchemaLaw::Validate | schema_builtins::SchemaLaw::Errors => {
                let [value, schema] = arguments.as_slice() else {
                    return Err(internal_contract("schema validation call without two arguments"));
                };
                let errors = schema_builtins::validate_errors(value, schema, resources)?;
                if matches!(law, schema_builtins::SchemaLaw::Validate) {
                    return Ok(Value::Bool(errors.is_empty()));
                }
                jqf_data::Array::try_from_vec(errors)
                    .map(Value::Array)
                    .map_err(|_| EngineRunError::allocation_failure())
            }
            schema_builtins::SchemaLaw::Diff => {
                let [value, schema] = arguments.as_slice() else {
                    return Err(internal_contract("schema_diff call without two arguments"));
                };
                schema_builtins::schema_diff_law(value, schema, resources)
            }
        })
    }

    #[cfg(feature = "ext-hash")]
    /// One ANALYTICS law: apply the pure/impure value law over the input, once
    /// per output of the count argument (`sample` only) under the shared
    /// [`Self::emit_argument_product`] law.
    pub(crate) fn eval_analytics(
        &mut self,
        law: extension_builtins::AnalyticsLaw,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        let ProgramNode::Call { args, .. } = &self.nodes[node.index()] else {
            return Err(internal_contract("analytics dispatch over a non-call node"));
        };
        // The SAMPLE located fast path: a small draw over a located array
        // materializes ONLY the drawn elements instead of the whole array.
        // Engages only for a LITERAL count — an argument that reads
        // the input needs the owned root the ordinary path builds — and
        // only for the small-draw half (n <= len/2), where the saving is
        // the whole array.
        if matches!(law, extension_builtins::AnalyticsLaw::Sample)
            && let Some(located) = input.located()
            && args.len() == 1
            && matches!(
                &self.nodes[args[0].index()],
                ProgramNode::Stage {
                    start: StageStart::Literal(_),
                    ..
                }
            )
        {
            let document = located.product().document();
            // The payload-transparent view: a tag LAYER is descended before the
            // node is asked for its array, so a tagged array engages the
            // small-draw fast path exactly as its payload would.
            let view = document
                .payload_view(located.node())
                .map_err(|_| internal_contract("analytics over a valid located document failed"))?;
            if let Some(array_view) = view
                .array()
                .map_err(|_| internal_contract("analytics over a valid located document failed"))?
            {
                let len = array_view.len();
                if len > 0 {
                    let draw_count = match &self.nodes[args[0].index()] {
                        ProgramNode::Stage {
                            start: StageStart::Literal(Value::Number(number)),
                            ..
                        } => number.to_i64().and_then(|value| usize::try_from(value).ok()),
                        _ => None,
                    };
                    if let Some(draw_n) = draw_count
                        && draw_n <= len / 2
                    {
                        // The seeded draw state lives on `resources`
                        // (`--seed`), the same seam `analytics_law`'s
                        // ordinary path reads: this fast path materializes
                        // the array DIFFERENTLY but must draw IDENTICALLY,
                        // or `--seed` would answer differently depending on
                        // whether the input happened to be located.
                        let positions = jqf_builtins::semantics::with_prng(resources, |rng| {
                            extension_builtins::draw_positions(rng, len, draw_n)
                        })?;
                        let mut out = Vec::new();
                        out.try_reserve_exact(draw_n)
                            .map_err(|_| EngineRunError::allocation_failure())?;
                        for position in positions {
                            let element = array_view.get(position);
                            let Some(element) = element else {
                                return Err(internal_contract(
                                    "located array index out of bounds under its own length",
                                ));
                            };
                            let handle = document
                                .node_handle(element.node())
                                .map_err(|_| internal_contract("analytics over a valid located document failed"))?;
                            let workspace = self.workspace.get_or_insert_with(jqf_data::MaterializeWorkspace::new);
                            let value = document
                                .materialize_node_with(workspace, handle, resources)
                                .map_err(|_| EngineRunError::allocation_failure())?;
                            out.push(value);
                        }
                        let value = jqf_data::Array::try_from_vec(out)
                            .map(Value::Array)
                            .map_err(|_| EngineRunError::allocation_failure())?;
                        return self.route(EngineResult::owned(value), cont, resources);
                    }
                }
            }
        }
        let source_node = input.located().map(LocatedProduct::node);
        let root = self.materialize_capture(input, resources)?;
        let slots = self.path_slots(&root, source_node, resources)?;
        self.emit_argument_product(args, &root, &slots, cont, resources, |arguments, resources| {
            extension_builtins::analytics_law(law, &root, arguments.first(), resources)
        })
    }

    #[cfg(feature = "ext-hash")]
    /// The RAND family: uniform floats/integers and uniform element choice.
    ///
    /// Every law with a filter argument evaluates it over the input by the
    /// shared [`Self::emit_argument_product`] law and draws once per
    /// combination. `rand/0` takes no argument and ignores the input entirely,
    /// so it routes WITHOUT materializing the input — a draw over a whole
    /// document must not pay the whole-document cost, which is why the
    /// argument-free form does not go through the shared law.
    pub(crate) fn eval_rand(
        &mut self,
        law: extension_builtins::RandLaw,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        let ProgramNode::Call { args: call_args, .. } = &self.nodes[node.index()] else {
            return Err(internal_contract("rand dispatch over a non-call node"));
        };
        if call_args.is_empty() {
            let value = extension_builtins::rand_law(law, &[], resources)?;
            return self.route(EngineResult::owned(value), cont, resources);
        }
        let source_node = input.located().map(LocatedProduct::node);
        let root = self.materialize_capture(input, resources)?;
        let slots = self.path_slots(&root, source_node, resources)?;
        self.emit_argument_product(call_args, &root, &slots, cont, resources, |arguments, resources| {
            extension_builtins::rand_law(law, &arguments, resources)
        })
    }

    /// The DIFF verb: apply the path-keyed semantic diff law to the call's two
    /// filter arguments (old, new) under the shared
    /// [`Self::emit_argument_product`] law, publishing one record array per
    /// combination.
    pub(crate) fn eval_diff(
        &mut self,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        let ProgramNode::Call { args, .. } = &self.nodes[node.index()] else {
            return Err(internal_contract("diff dispatch over a non-call node"));
        };
        let source_node = input.located().map(LocatedProduct::node);
        let root = self.materialize_capture(input, resources)?;
        let slots = self.path_slots(&root, source_node, resources)?;
        self.emit_argument_product(args, &root, &slots, cont, resources, |mut arguments, resources| {
            let (Some(new), Some(old)) = (arguments.pop(), arguments.pop()) else {
                return Err(internal_contract("diff call without two arguments"));
            };
            jqf_builtins::registry::builtins::diff::diff_law(old, new, resources)
        })
    }

    /// One TOP-K partial sort: materialize the input array, evaluate the optional
    /// projection filter per element, then build a bounded-size heap (O(n log k))
    /// and answer the k smallest.
    ///
    /// The COUNT argument fans out under the shared [`Self::emit_argument_product`]
    /// law, exactly as every other argument-taking call's does: `top_k(1, 2)` is
    /// TWO answers, and `top_k(empty)` is none. Reading only the last output of
    /// the count filter — which is what this drive did before — is the
    /// argument-degeneracy the SDK's `builtin_examples` audit exists to catch.
    ///
    /// The `by` projection is NOT part of that product: it is applied per
    /// ELEMENT, not per call, so it is evaluated once here and shared across
    /// every count in the fan-out. That also puts the not-sortable check ahead
    /// of the count, which is the `sort_by(f) | .[0:n]` order — `5 | top_k(empty)`
    /// raises, because the sort would.
    pub(crate) fn eval_topk(
        &mut self,
        law: top_k_builtins::TopKDirection,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let (count_graph, by_graph) = match &self.nodes[node.index()] {
            ProgramNode::Call { args, .. } => {
                let count_arg = args
                    .first()
                    .copied()
                    .ok_or_else(|| internal_contract("top_k call with no count argument"))?;
                let by = args.get(1).copied();
                (count_arg, by)
            }
            _ => return Err(internal_contract("top_k dispatch over a non-call node")),
        };
        self.raise_cont = cont;
        let source_node = input.located().map(LocatedProduct::node);
        let root = self.materialize_capture(input, resources)?;
        let slots = self.path_slots(&root, source_node, resources)?;

        let array = match root.untagged() {
            Value::Array(array) => array,
            other => {
                let operand = message::dump_trunc_owned(other)?;
                let text = message::not_sortable_message(other.kind(), &operand)?;
                return Err(jqf_builtins::semantics::path::raise(&text, resources));
            }
        };
        let mut projection_keys: Vec<alloc::vec::Vec<Value>> = Vec::new();
        if let Some(by_g) = by_graph {
            projection_keys
                .try_reserve_exact(array.len())
                .map_err(|_| EngineRunError::allocation_failure())?;
            let nodes = self.nodes;
            for position in 0..array.len() {
                let child = keyed_child(&root, position)?;
                let overlay = clone_slots(&slots);
                let outputs = path_register::evaluate_owned_filter(
                    nodes,
                    resources,
                    overlay,
                    by_g,
                    child,
                    self.topk,
                    &mut self.fact_deltas,
                )?;
                projection_keys.push(outputs);
            }
        }

        self.emit_argument_product(
            core::slice::from_ref(&count_graph),
            &root,
            &slots,
            cont,
            resources,
            |arguments, resources| {
                let Some(count_value) = arguments.first() else {
                    return Err(internal_contract("top_k product without a count"));
                };
                top_k_builtins::top_k_law(law, &root, count_value, &projection_keys, resources)
            },
        )
    }

    #[cfg(feature = "ext-hash")]
    /// One jqf-extension law: apply the pure value law to the input and ONE
    /// combination of the call's filter arguments, under the shared
    /// [`Self::emit_argument_product`] law.
    ///
    /// Which parameter each law reads is
    /// [`extension_builtins::extension_law`]'s fact, not this drive's: this
    /// drive owns only the argument EVALUATION, so path mode's eager run and
    /// this frame drive cannot disagree about `log/2`'s base or `round/2`'s
    /// digit count.
    pub(crate) fn eval_extension(
        &mut self,
        law: extension_builtins::ExtensionLaw,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        let ProgramNode::Call { args, .. } = &self.nodes[node.index()] else {
            return Err(internal_contract("extension dispatch over a non-call node"));
        };
        let source_node = input.located().map(LocatedProduct::node);
        let root = self.materialize_capture(input, resources)?;
        let slots = self.path_slots(&root, source_node, resources)?;
        self.emit_argument_product(args, &root, &slots, cont, resources, |arguments, resources| {
            extension_builtins::extension_law(law, &root, &arguments, resources)
        })
    }

    #[cfg(feature = "ext-net")]
    /// Dispatches one IP/CIDR law over its call node.
    ///
    /// The four arity-0 laws run over the materialized input alone; `ip_in_cidr`
    /// evaluates its CIDR argument filter over the same input (the
    /// argument-evaluation law) and answers once per argument output through the
    /// argument-product drive, exactly like [`Self::eval_extension`].
    pub(crate) fn eval_net(
        &mut self,
        law: net_builtins::NetLaw,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        let ProgramNode::Call { args, .. } = &self.nodes[node.index()] else {
            return Err(internal_contract("net dispatch over a non-call node"));
        };
        let source_node = input.located().map(LocatedProduct::node);
        let root = self.materialize_capture(input, resources)?;
        let slots = self.path_slots(&root, source_node, resources)?;
        self.emit_argument_product(args, &root, &slots, cont, resources, |arguments, resources| {
            net_builtins::net_law(law, &root, &arguments, resources)
        })
    }

    /// Opens the argument nesting for one /2 or /3 math call.
    ///
    /// The RIGHTMOST argument is the outer loop (the right-outer Cartesian
    /// law, shared with [`Self::eval_binary`]): the retained input re-borrows
    /// per nested evaluation, and each complete outer combination drives the
    /// FIRST argument before the next outer output resumes.
    pub(crate) fn eval_math_args(
        &mut self,
        law: MathCallLaw,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let ProgramNode::Call { args, .. } = &self.nodes[node.index()] else {
            return Err(internal_contract("math dispatch over a non-call node"));
        };
        let inner = *args
            .first()
            .ok_or_else(|| internal_contract("math call with no arguments"))?;
        let rightmost = *args
            .last()
            .ok_or_else(|| internal_contract("math call with no arguments"))?;
        let outer = args[1..].to_vec();
        let retained = input.try_clone().map_err(EngineRunError::Codec)?;
        self.push_frame(GraphFrame::MathArgs {
            law,
            outer,
            collected: Vec::new(),
            inner,
            input: retained,
            out_cont: cont,
        });
        self.pending = Some(GraphTask::Eval {
            node: rightmost,
            input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Consumes one output of the current OUTER argument and either descends
    /// to the next outer argument or opens the innermost phase.
    ///
    /// Each output builds its own nested frame, mirroring
    /// [`Self::consume_binary_right`]: the frame being consumed stays below
    /// the nested evaluation and pops silently when that evaluation completes,
    /// which is what lets the argument generator's continuation produce its
    /// next output.
    pub(crate) fn consume_math_args(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let owned = self.materialize_capture(value, resources)?;
        // The frame PERSISTS across the argument graph's outputs: each output
        // of the current outer argument builds its own nested drive from a
        // COPY of the frame's state (the prefix is at most two values), and
        // the next output re-enters this same frame with its original `outer`
        // intact. The nested evaluation's frames sit ABOVE the argument
        // graph's own continuation, so when it completes the graph resumes and
        // routes the next output back here — the same shape `range`'s bound
        // nesting and `BinaryRight` use.
        let (law, outer, collected, inner, input, out_cont) = match self.frames.as_slice().get(index) {
            Some(GraphFrame::MathArgs {
                law,
                outer,
                collected,
                inner,
                input,
                out_cont,
            }) => {
                let mut prefix = Vec::new();
                prefix
                    .try_reserve(collected.len() + 1)
                    .map_err(|_| EngineRunError::allocation_failure())?;
                for value in collected {
                    prefix.push(value.clone());
                }
                prefix.push(owned);
                (
                    *law,
                    outer.clone(),
                    prefix,
                    *inner,
                    input.try_clone().map_err(EngineRunError::Codec)?,
                    *out_cont,
                )
            }
            _ => {
                return Err(internal_contract("math args frame kind changed under capture"));
            }
        };
        let mut outer = outer;
        outer
            .pop()
            .ok_or_else(|| internal_contract("math args frame consumed past its last argument"))?;
        self.raise_cont = out_cont;
        if let Some(next) = outer.last().copied() {
            let retained = input.try_clone().map_err(EngineRunError::Codec)?;
            self.push_frame(GraphFrame::MathArgs {
                law,
                outer,
                collected,
                inner,
                input: retained,
                out_cont,
            });
            self.pending = Some(GraphTask::Eval {
                node: next,
                input,
                cont: self.frames.len(),
            });
            return Ok(None);
        }
        // Every outer argument is collected (rightmost first): drive the FIRST
        // argument, publishing one result per output.
        self.push_frame(GraphFrame::MathCompute {
            law,
            outer: collected,
            out_cont,
        });
        self.pending = Some(GraphTask::Eval {
            node: inner,
            input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Consumes one output of the FIRST argument and publishes the computed
    /// result for the collected outer values.
    pub(crate) fn consume_math_compute(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let inner = self.materialize_capture(value, resources)?;
        // The compute frame PERSISTS across the first argument's outputs: each
        // output combines the same outer values, so the prefix is copied per
        // output (at most two values) rather than taken.
        let (law, outer, out_cont) = match self.frames.as_slice().get(index) {
            Some(GraphFrame::MathCompute { law, outer, out_cont }) => {
                let mut prefix = Vec::new();
                prefix
                    .try_reserve(outer.len())
                    .map_err(|_| EngineRunError::allocation_failure())?;
                for value in outer {
                    prefix.push(value.clone());
                }
                (*law, prefix, *out_cont)
            }
            _ => {
                return Err(internal_contract("math compute frame kind changed under capture"));
            }
        };
        self.raise_cont = out_cont;
        let answer = match law {
            MathCallLaw::Bin(bin) => {
                let [rhs] = outer.as_slice() else {
                    return Err(internal_contract("math bin compute with wrong outer arity"));
                };
                math_builtins::apply_bin(bin, &inner, rhs, resources)?
            }
            MathCallLaw::Tern(tern) => {
                let [z, y] = outer.as_slice() else {
                    return Err(internal_contract("math tern compute with wrong outer arity"));
                };
                math_builtins::apply_tern(tern, &inner, y, z, resources)?
            }
        };
        let result = self.retain_owned(answer);
        self.route(result, out_cont, resources)
    }

    /// Drives `walk(f)`: open the bottom-up rebuild over the whole input.
    pub(crate) fn eval_walk(
        &mut self,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let filter = match &self.nodes[node.index()] {
            ProgramNode::Call { args, .. } => *args
                .first()
                .ok_or_else(|| internal_contract("walk/1 call with no argument"))?,
            _ => return Err(internal_contract("walk dispatch over a non-call node")),
        };
        self.raise_cont = cont;
        let subject = self.materialize_capture(input, resources)?;
        self.open_walk(subject, filter, cont, resources)?;
        Ok(None)
    }

    /// Descends to the first value `f` actually runs on, pushing one
    /// [`GraphFrame::Walk`] per container on the way down.
    ///
    /// The descent is a LOOP and not a recursion: `walk` over a
    /// ten-thousand-deep document is a frame-stack depth, never a Rust one. A
    /// scalar — or an EMPTY container, whose rebuild is equal to itself — is
    /// where the descent stops and `f` is seeded.
    pub(crate) fn open_walk(
        &mut self,
        subject: Value,
        filter: ProgramNodeId,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        let mut value = subject;
        let mut cont = cont;
        loop {
            let (width, rebuild) = match value.untagged() {
                Value::Array(array) if !array.is_empty() => (
                    array.len(),
                    WalkRebuild::Array(
                        Array::try_with_capacity(array.len()).map_err(|_| EngineRunError::allocation_failure())?,
                    ),
                ),
                Value::Object(object) if !object.is_empty() => (
                    object.len(),
                    WalkRebuild::Object {
                        entries: ObjectBuilder::try_with_capacity(object.len())
                            .map_err(|_| EngineRunError::allocation_failure())?,
                        taken: false,
                    },
                ),
                _ => {
                    // A SCALAR passes through the walk unchanged — its value
                    // is the register's, so it publishes (`path(walk(.))`
                    // over `5` is `[]`); an empty CONTAINER is rebuilt, so it
                    // fails the register check.
                    if self.path.is_some() && matches!(value.untagged(), Value::Array(_) | Value::Object(_)) {
                        return Err(jqf_builtins::semantics::path::invalid_result(&value, resources));
                    }
                    self.pending = Some(GraphTask::Eval {
                        node: filter,
                        input: EngineResult::owned(value),
                        cont,
                    });
                    return Ok(());
                }
            };
            let child = keyed_child(&value, 0)?;
            self.push_frame(GraphFrame::Walk {
                subject: value,
                width,
                cursor: 0,
                rebuild,
                filter,
                out_cont: cont,
            });
            cont = self.frames.len();
            value = child;
        }
    }

    /// Consumes one output of the current child's walk.
    ///
    /// The ARRAY arm collects every output. The OBJECT arm takes the first and
    /// CUTS: the frames the child's remaining outputs would come from are
    /// dropped, which is the `.[] |= f` cap and is what makes
    /// `{"a":1} | walk(if type == "number" then (., error("x")) else . end)`
    /// answer `{"a":1}` where the array spelling raises.
    pub(crate) fn consume_walk(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let owned = self.materialize_capture(value, resources)?;
        let key = match self.frames.as_slice().get(index) {
            Some(GraphFrame::Walk {
                subject,
                cursor,
                rebuild: WalkRebuild::Object { taken: false, .. },
                ..
            }) => Some(walk_key(subject, *cursor)?),
            Some(GraphFrame::Walk { .. }) => None,
            _ => return Err(internal_contract("walk frame kind changed under capture")),
        };
        let Some(GraphFrame::Walk { rebuild, .. }) = self.frames.as_mut_slice().get_mut(index) else {
            return Err(internal_contract("walk frame kind changed under capture"));
        };
        match rebuild {
            WalkRebuild::Array(array) => {
                return array
                    .try_push(owned)
                    .map(|()| None)
                    .map_err(|_| EngineRunError::allocation_failure());
            }
            WalkRebuild::Object { taken: true, .. } => return Ok(None),
            WalkRebuild::Object { entries, taken } => {
                let key = key.ok_or_else(|| internal_contract("a walk key that was not read"))?;
                entries
                    .try_insert_last(key, owned)
                    .map_err(|_| EngineRunError::allocation_failure())?;
                *taken = true;
            }
        }
        // The cut: everything the child's walk still had to say is dropped, and
        // the frame moves straight to the next key.
        while self.frames.as_slice().len() > index + 1 {
            self.pop_frame_releasing(resources);
        }
        self.advance_walk(resources).map(|_| None)
    }

    /// Closes the current child and starts the next, or finishes the container.
    ///
    /// Finishing is the second half of the `w` law: the rebuilt container is run
    /// through `f`, at the frame's own out-continuation, so a fan-out there
    /// reaches the parent exactly as a child's fan-out does.
    pub(crate) fn advance_walk(&mut self, resources: &mut ResourceContext<'_>) -> Result<bool, EngineRunError> {
        let top = self.frames.as_slice().len() - 1;
        let started = {
            let Some(GraphFrame::Walk {
                subject,
                width,
                cursor,
                rebuild,
                ..
            }) = self.frames.as_mut_slice().get_mut(top)
            else {
                return Err(internal_contract("walk frame kind changed under advance"));
            };
            *cursor += 1;
            if let WalkRebuild::Object { taken, .. } = rebuild {
                *taken = false;
            }
            if *cursor >= *width {
                None
            } else {
                Some(keyed_child(subject, *cursor)?)
            }
        };
        if let Some(child) = started {
            let (filter, cont) = match self.frames.as_slice().get(top) {
                Some(GraphFrame::Walk { filter, .. }) => (*filter, top + 1),
                _ => return Err(internal_contract("walk frame kind changed under advance")),
            };
            self.open_walk(child, filter, cont, resources)?;
            return Ok(true);
        }
        let Some(GraphFrame::Walk {
            rebuild,
            filter,
            out_cont,
            ..
        }) = self.frames.pop()
        else {
            return Err(internal_contract("walk frame kind changed under pop"));
        };
        self.raise_cont = out_cont;
        // Keys came from the source object at unique cursor positions.
        // A source object never has duplicate keys, so uniqueness is
        // already proved and last-value-wins does not apply.
        let container = match rebuild {
            WalkRebuild::Array(array) => Value::Array(array),
            WalkRebuild::Object { entries, .. } => entries
                .try_finish_unique()
                .map(Value::Object)
                .map_err(|_| EngineRunError::allocation_failure())?,
        };
        // The walk's output is computed: inside a path body it always fails
        // the register check.
        if self.path.is_some() {
            return Err(jqf_builtins::semantics::path::invalid_result(&container, resources));
        }
        self.pending = Some(GraphTask::Eval {
            node: filter,
            input: EngineResult::owned(container),
            cont: out_cont,
        });
        Ok(true)
    }

    /// Opens `map_values(f)` over the materialized input: seed the first
    /// member's update, or publish the empty-container / scalar law directly.
    ///
    /// The drive is `walk`'s OBJECT arm, single-level: `f` runs over each
    /// member of the TOP container exactly once (no descent), the FIRST output
    /// per member wins and the rest are cut, and a member whose update
    /// produced nothing is DELETED. Unlike [`Self::open_walk`], the rebuilt
    /// container is NOT passed through `f` again — `map_values(f)` is
    /// `.[] |= f`, whose rebuild is the output.
    pub(crate) fn eval_map_values(
        &mut self,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let filter = match &self.nodes[node.index()] {
            ProgramNode::Call { args, .. } => *args
                .first()
                .ok_or_else(|| internal_contract("map_values/1 call with no argument"))?,
            _ => {
                return Err(internal_contract("map_values dispatch over a non-call node"));
            }
        };
        self.raise_cont = cont;
        let subject = self.materialize_capture(input, resources)?;
        self.open_map_values(subject, filter, cont, resources)?;
        Ok(None)
    }

    /// Seeds the first member's update, or publishes the empty/scalar law.
    ///
    /// A NON-empty container pushes a [`GraphFrame::MapValues`] frame and
    /// evaluates `f` over its first child; an EMPTY container is its own
    /// rebuild (`{} | map_values(f)` is `{}`); a scalar raises the ordinary
    /// iterate mismatch (`Cannot iterate over number (5)`), payload-transparent
    /// through any tag layers.
    pub(crate) fn open_map_values(
        &mut self,
        subject: Value,
        filter: ProgramNodeId,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        let (width, rebuild) = match subject.untagged() {
            Value::Array(array) if !array.is_empty() => (
                array.len(),
                MapValuesRebuild::Array {
                    items: Array::try_with_capacity(array.len()).map_err(|_| EngineRunError::allocation_failure())?,
                    taken: false,
                },
            ),
            Value::Object(object) if !object.is_empty() => (
                object.len(),
                MapValuesRebuild::Object {
                    entries: ObjectBuilder::try_with_capacity(object.len())
                        .map_err(|_| EngineRunError::allocation_failure())?,
                    taken: false,
                },
            ),
            Value::Array(_) | Value::Object(_) => {
                // An empty container is its own rebuild; a tagged one keeps its
                // tag (the `.[] |= f` update law).
                let container = match subject.tag() {
                    None => subject,
                    Some(tag) => {
                        let tag = tag.clone();
                        Value::try_tagged(tag, subject).map_err(|_| EngineRunError::allocation_failure())?
                    }
                };
                let result = self.retain_owned(container);
                self.pending = Some(GraphTask::Route { value: result, cont });
                return Ok(());
            }
            other => {
                let operand = message::dump_trunc_owned(other)?;
                let text = message::iterate_message(other.kind(), &operand)?;
                return Err(jqf_builtins::semantics::path::raise(&text, resources));
            }
        };
        let child = keyed_child(&subject, 0)?;
        self.push_frame(GraphFrame::MapValues {
            subject,
            width,
            cursor: 0,
            rebuild,
            filter,
            out_cont: cont,
        });
        self.pending = Some(GraphTask::Eval {
            node: filter,
            input: EngineResult::owned(child),
            cont: self.frames.len(),
        });
        Ok(())
    }

    /// Consumes one output of the current member's update.
    ///
    /// The FIRST output per member lands in the rebuild; a later output of the
    /// SAME member's update is dropped (`taken`), which is the `.[] |= f` cap
    /// and is what makes `{"a":1} | map_values(., error("x"))` answer
    /// `{"a":1}` rather than raising. An update that produced NOTHING never
    /// reaches this arm — the member is simply absent from the rebuild, which
    /// is the `|= empty` deletion.
    pub(crate) fn consume_map_values(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let owned = self.materialize_capture(value, resources)?;
        let key = match self.frames.as_slice().get(index) {
            Some(GraphFrame::MapValues {
                subject,
                cursor,
                rebuild: MapValuesRebuild::Object { taken: false, .. },
                ..
            }) => Some(walk_key(subject, *cursor)?),
            Some(GraphFrame::MapValues { .. }) => None,
            _ => {
                return Err(internal_contract("map_values frame kind changed under capture"));
            }
        };
        let Some(GraphFrame::MapValues { rebuild, .. }) = self.frames.as_mut_slice().get_mut(index) else {
            return Err(internal_contract("map_values frame kind changed under capture"));
        };
        match rebuild {
            MapValuesRebuild::Array { taken: true, .. } | MapValuesRebuild::Object { taken: true, .. } => {
                return Ok(None);
            }
            MapValuesRebuild::Array { items, taken } => {
                items
                    .try_push(owned)
                    .map_err(|_| EngineRunError::allocation_failure())?;
                *taken = true;
            }
            MapValuesRebuild::Object { entries, taken } => {
                let key = key.ok_or_else(|| internal_contract("a map_values key that was not read"))?;
                entries
                    .try_insert_last(key, owned)
                    .map_err(|_| EngineRunError::allocation_failure())?;
                *taken = true;
            }
        }
        // The cut: everything the current member's update still had to say is
        // dropped, and the frame moves straight to the next member.
        while self.frames.as_slice().len() > index + 1 {
            self.pop_frame_releasing(resources);
        }
        self.advance_map_values(resources).map(|_| None)
    }

    /// Closes the current member and starts the next, or publishes the rebuilt
    /// container.
    ///
    /// Finishing is the one law that differs from [`Self::advance_walk`]: the
    /// rebuilt container routes through `out_cont` DIRECTLY, never through `f`
    /// again.
    pub(crate) fn advance_map_values(&mut self, _resources: &mut ResourceContext<'_>) -> Result<bool, EngineRunError> {
        let top = self.frames.as_slice().len() - 1;
        let started = {
            let Some(GraphFrame::MapValues {
                subject,
                width,
                cursor,
                rebuild,
                ..
            }) = self.frames.as_mut_slice().get_mut(top)
            else {
                return Err(internal_contract("map_values frame kind changed under advance"));
            };
            *cursor += 1;
            match rebuild {
                MapValuesRebuild::Array { taken, .. } | MapValuesRebuild::Object { taken, .. } => {
                    *taken = false;
                }
            }
            if *cursor >= *width {
                None
            } else {
                Some(keyed_child(subject, *cursor)?)
            }
        };
        if let Some(child) = started {
            let (filter, cont) = match self.frames.as_slice().get(top) {
                Some(GraphFrame::MapValues { filter, .. }) => (*filter, top + 1),
                _ => {
                    return Err(internal_contract("map_values frame kind changed under advance"));
                }
            };
            self.pending = Some(GraphTask::Eval {
                node: filter,
                input: EngineResult::owned(child),
                cont,
            });
            return Ok(true);
        }
        let Some(GraphFrame::MapValues {
            subject,
            rebuild,
            out_cont,
            ..
        }) = self.frames.pop()
        else {
            return Err(internal_contract("map_values frame kind changed under pop"));
        };
        self.raise_cont = out_cont;
        // Keys came from the source object at unique cursor positions.
        // Deletion drops a member; it never mints a second copy of a key.
        let mut container = match rebuild {
            MapValuesRebuild::Array { items, .. } => Value::Array(items),
            MapValuesRebuild::Object { entries, .. } => entries
                .try_finish_unique()
                .map(Value::Object)
                .map_err(|_| EngineRunError::allocation_failure())?,
        };
        // The tag law: `map_values` is `.[] |= f`, a path update, so a tagged
        // container keeps its tag (`Value::Tagged` layers are payload-
        // transparent through the drive).
        if let Some(tag) = subject.tag() {
            let tag = tag.clone();
            container = Value::try_tagged(tag, container).map_err(|_| EngineRunError::allocation_failure())?;
        }
        let result = self.retain_owned(container);
        self.pending = Some(GraphTask::Route {
            value: result,
            cont: out_cont,
        });
        Ok(true)
    }
}

/// Parse each outer-loop path, then match it against every source.
///
/// Path/query is the outer loop of the right-outer Cartesian argument law
/// ([`GraphMachine::eval_pointer`]); a parse or match error latches and
/// stops so already-collected arrays still emit.
fn navigate_parsed_paths<Parsed>(
    paths: &[Value],
    sources: &[Value],
    resources: &mut ResourceContext<'_>,
    mut parse: impl FnMut(&Value, &mut ResourceContext<'_>) -> Result<Parsed, EngineRunError>,
    mut match_one: impl FnMut(&Value, &Parsed, &mut ResourceContext<'_>) -> Result<Value, EngineRunError>,
) -> Result<(Vec<Value>, Option<EngineRunError>), EngineRunError> {
    let mut values = Vec::new();
    let mut pending = None;
    'outer: for path in paths {
        let parsed = match parse(path, resources) {
            Ok(parsed) => parsed,
            Err(error) => {
                pending = Some(error);
                break;
            }
        };
        for source in sources {
            match match_one(source, &parsed, resources) {
                Ok(array) => {
                    values
                        .try_reserve(1)
                        .map_err(|_| EngineRunError::allocation_failure())?;
                    values.push(array);
                }
                Err(error) => {
                    pending = Some(error);
                    break 'outer;
                }
            }
        }
    }
    Ok((values, pending))
}
