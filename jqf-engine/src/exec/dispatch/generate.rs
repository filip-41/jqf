//! Generator family: range, while/until, repeat, recurse, combinations.
//!
//! One job: open a `Generate` frame and resume it. Values come from advancing
//! that frame, never from an eager loop in call dispatch.

#[allow(clippy::wildcard_imports)]
use super::super::*;

impl<'source> GraphMachine<'_, 'source> {
    // ---- the generator family -------------------------------------------
    //
    // Six sources, one frame. Each drive below opens a
    // [`GenerateState`] and then does nothing further: the frame stack IS the
    // generator's stack, so a resumption is an `advance_generate` arm and a
    // recursion is a pushed frame, never a Rust call.

    /// Opens the generator the dispatch named.
    ///
    /// One arm per row of [`generate::GENERATORS`], and the only place the six
    /// rows are told apart at eval time.
    pub(crate) fn eval_generate(
        &mut self,
        generator: Generator,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        match generator {
            Generator::Range => self.eval_range(node, input, cont),
            Generator::While => self.eval_condition_loop(LoopForm::While, node, input, cont, resources),
            Generator::Until => self.eval_condition_loop(LoopForm::Until, node, input, cont, resources),
            Generator::Repeat => self.eval_repeat(node, input, cont, resources),
            Generator::Recurse => self.eval_recurse(node, input, cont, resources),
            Generator::Combinations => self.eval_combinations(node, input, cont, resources),
        }
    }

    /// Opens `range`'s LEFT-OUTER bound nesting over the call's first argument.
    ///
    /// The bounds are consumed one at a time rather than collected, because the
    /// nesting is observable: `range(0, error("x"); 3)` emits `0 1 2` and only
    /// then raises, which an eager collection of `from`'s outputs would invert.
    pub(crate) fn eval_range(
        &mut self,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        let first = self.call_arg(node, 0)?;
        let retained = input.try_clone().map_err(EngineRunError::Codec)?;
        self.push_frame(GraphFrame::Generate {
            state: GenerateState::RangeBound {
                node,
                depth: 0,
                // Empty: `range(upto)`'s `from` default of 0 and every
                // arity's `by` default of 1 are applied by
                // `generate::open`, once the authored bounds are all in.
                bounds: RangeBounds::new(),
                input: retained,
                out_cont: cont,
            },
        });
        self.pending = Some(GraphTask::Eval {
            node: first,
            input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Fixes one `range` bound and either descends to the next one or opens the
    /// emitter.
    ///
    /// The bound is RETAINED rather than converted here, because the law it
    /// belongs to is a fact about the whole triple: `generate::open` picks the
    /// `f64` accumulator or the value cursor once the last bound lands, and it
    /// is also where arity 1 and 2 take the type check — at the moment the
    /// opcode takes it, with every bound already evaluated.
    pub(crate) fn consume_range_bound(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let owned = self.materialize_capture(value, resources)?;
        let (node, depth, mut bounds, input, out_cont) = match self.frames.as_slice().get(index) {
            Some(GraphFrame::Generate {
                state:
                    GenerateState::RangeBound {
                        node,
                        depth,
                        bounds,
                        input,
                        out_cont,
                    },
            }) => (
                *node,
                *depth,
                bounds.try_clone(),
                input.try_clone().map_err(EngineRunError::Codec)?,
                *out_cont,
            ),
            _ => return Err(internal_contract("range frame kind changed under capture")),
        };
        self.raise_cont = out_cont;
        let arity = self.call_arity(node)?;
        bounds.set(range_slot(arity, depth), owned)?;
        if depth + 1 < arity {
            let next = self.call_arg(node, depth + 1)?;
            let retained = input.try_clone().map_err(EngineRunError::Codec)?;
            self.push_frame(GraphFrame::Generate {
                state: GenerateState::RangeBound {
                    node,
                    depth: depth + 1,
                    bounds,
                    input: retained,
                    out_cont,
                },
            });
            self.pending = Some(GraphTask::Eval {
                node: next,
                input,
                cont: self.frames.len(),
            });
            return Ok(None);
        }
        let state = match generate::open(bounds, arity, resources)? {
            RangeLaw::Numeric { cursor, first } => GenerateState::Range {
                cursor,
                first,
                out_cont,
            },
            RangeLaw::Value(cursor) => GenerateState::ValueRange { cursor, out_cont },
        };
        self.push_frame(GraphFrame::Generate { state });
        Ok(None)
    }

    /// Drives `while`/`until`: materialize the input, then test the condition.
    pub(crate) fn eval_condition_loop(
        &mut self,
        form: LoopForm,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        let test = self.call_arg(node, 0)?;
        let update = self.call_arg(node, 1)?;
        let current = self.materialize_capture(input, resources)?;
        self.start_loop(form, current, test, update, cont);
        Ok(None)
    }

    /// Opens one loop round: the condition, over the value the round starts at.
    pub(crate) fn start_loop(
        &mut self,
        form: LoopForm,
        current: Value,
        cond: ProgramNodeId,
        update: ProgramNodeId,
        out_cont: usize,
    ) {
        let seed = current.clone();
        self.push_frame(GraphFrame::Generate {
            state: GenerateState::LoopCond {
                form,
                current,
                cond,
                update,
                out_cont,
            },
        });
        self.pending = Some(GraphTask::Eval {
            node: cond,
            input: EngineResult::owned(seed),
            cont: self.frames.len(),
        });
    }

    /// Applies one condition output to the round's retained value.
    ///
    /// `while` emits FIRST and steps after (`def _while: if cond then .,
    /// (update | _while) else empty end`), so the step is deferred into its own
    /// frame rather than started here — the emission has to reach its consumers
    /// before the next value exists. `until` is the mirror: a truthy condition
    /// emits and STOPS, a falsy one steps and emits nothing.
    pub(crate) fn consume_loop_cond(
        &mut self,
        index: usize,
        value: &EngineResult<'source>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let truthy = truth::is_truthy(value).map_err(|_| internal_contract("loop condition truth check failed"))?;
        let (form, current, cond, update, out_cont) = match self.frames.as_slice().get(index) {
            Some(GraphFrame::Generate {
                state:
                    GenerateState::LoopCond {
                        form,
                        current,
                        cond,
                        update,
                        out_cont,
                    },
            }) => (*form, current.clone(), *cond, *update, *out_cont),
            _ => return Err(internal_contract("loop frame kind changed under capture")),
        };
        match (form, truthy) {
            (LoopForm::While, true) => {
                let carried = current.clone();
                self.push_frame(GraphFrame::Generate {
                    state: GenerateState::LoopStep {
                        current: carried,
                        cond,
                        update,
                        out_cont,
                        cond_index: index,
                    },
                });
                self.pending = Some(GraphTask::Route {
                    value: EngineResult::owned(current),
                    cont: out_cont,
                });
                Ok(None)
            }
            (LoopForm::While, false) => Ok(None),
            (LoopForm::Until, true) => {
                self.pending = Some(GraphTask::Route {
                    value: EngineResult::owned(current),
                    cont: out_cont,
                });
                Ok(None)
            }
            (LoopForm::Until, false) => {
                self.push_frame(GraphFrame::Generate {
                    state: GenerateState::LoopUpdate {
                        form,
                        cond,
                        update,
                        out_cont,
                        cond_index: index,
                    },
                });
                self.pending = Some(GraphTask::Eval {
                    node: update,
                    input: EngineResult::owned(current),
                    cont: self.frames.len(),
                });
                Ok(None)
            }
        }
    }

    /// Starts one loop round per update output.
    pub(crate) fn consume_loop_update(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let current = self.materialize_capture(value, resources)?;
        let (form, cond, update, out_cont, cond_index) = match self.frames.as_slice().get(index) {
            Some(GraphFrame::Generate {
                state:
                    GenerateState::LoopUpdate {
                        form,
                        cond,
                        update,
                        out_cont,
                        cond_index,
                    },
            }) => (*form, *cond, *update, *out_cont, *cond_index),
            _ => return Err(internal_contract("loop frame kind changed under capture")),
        };
        self.raise_cont = out_cont;
        if self.tail_step_loop(form, index, cond_index, resources)? {
            // The retired round left its own `LoopCond` slot standing at
            // `cond_index` and nothing above it. The next round's frame is the
            // same kind at the same height, so it is WRITTEN OVER the old one
            // rather than popped and pushed back: one 248-byte frame move per
            // round instead of three, and the assignment runs the old frame's
            // drop glue exactly where the pop would have.
            let seed = current.clone();
            let Some(slot) = self.frames.as_mut_slice().get_mut(cond_index) else {
                return Err(internal_contract("loop cond frame vanished under reuse"));
            };
            *slot = GraphFrame::Generate {
                state: GenerateState::LoopCond {
                    form,
                    current,
                    cond,
                    update,
                    out_cont,
                },
            };
            self.pending = Some(GraphTask::Eval {
                node: cond,
                input: EngineResult::owned(seed),
                cont: cond_index.saturating_add(1),
            });
            return Ok(None);
        }
        self.start_loop(form, current, cond, update, out_cont);
        Ok(None)
    }

    /// Retires the round that just produced this update output, when it has
    /// nothing left to produce — the loop family's TAIL STEP.
    ///
    /// A round is two consumer frames, the condition's and the update's, and
    /// they exist to hold the round's remaining ALTERNATIVES: a second condition
    /// output re-runs the `if cond then ., (update | _while) else empty end`
    /// body, and a second update output starts a second next round. The common
    /// loop has neither — one condition output, one update output — and then the
    /// two frames hold nothing at all, yet the machine used to stack round N+1's
    /// pair on top of them and unwind the whole tower only when the loop ended.
    /// That is what made `[while(.<N; .+1)]` cost O(N) frames and die of the
    /// memory ceiling at a few hundred thousand rounds while the flat loop
    /// runs on.
    ///
    /// The law: A TAIL STEP IS ITERATION, NOT NESTING. When a round's producers
    /// are spent, the round is over, and the next round belongs at the SAME
    /// stack height rather than one level deeper. Nothing here decides whether
    /// they are spent — [`Self::trim_spent_frames`] OBSERVES it, and a round
    /// with genuine fan-out (a multi-output condition, a multi-output update, a
    /// generator still live in either) simply fails the observation and keeps
    /// every frame it has. So the tree-shaped loop is untouched and the linear
    /// one is flat.
    ///
    /// The two pops are independent: the update frame can retire while the
    /// condition frame stays (a condition generator that is still live sits
    /// between them), and that is a correct half-step, not a missed one.
    pub(crate) fn tail_step_loop(
        &mut self,
        form: LoopForm,
        index: usize,
        cond_index: usize,
        resources: &ResourceContext<'_>,
    ) -> Result<bool, EngineRunError> {
        if !self.trim_spent_frames(index.saturating_add(1), resources) {
            return Ok(false);
        }
        // The update consumer is now the top frame and its generator is spent:
        // this output was the last one it will ever route.
        self.frames.pop();
        // The condition frame is RETAINED rather than popped when it is spent
        // and top: the caller writes the next round's frame over it. Retiring
        // it and reporting `false` would be equally correct and one frame move
        // more expensive — the observation is the same either way.
        let reusable = self.trim_spent_frames(cond_index.saturating_add(1), resources)
            && matches!(
                self.frames.as_slice().get(cond_index),
                Some(GraphFrame::Generate {
                    state: GenerateState::LoopCond { .. }
                })
            );
        // A retired `until` round is the loop's ONLY cooperation point: `while`
        // still defers through `LoopStep`, whose resumption already cooperates
        // ([`Self::cooperate`]), but `until` leaves no frame for
        // [`Self::advance_generate`] to resume at all — so an `until` whose
        // condition never comes true would spin with neither a growing stack
        // nor a deadline check. Checking on the `while` path too would only
        // double a check that round already pays.
        if matches!(form, LoopForm::Until) {
            resources
                .check_control()
                .map_err(|error| EngineRunError::from(EngineError::Control(error)))?;
        }
        Ok(reusable)
    }

    /// Drives `repeat`: retain the input and open the round producer.
    pub(crate) fn eval_repeat(
        &mut self,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        let body = self.call_arg(node, 0)?;
        let seed = self.materialize_capture(input, resources)?;
        self.push_frame(GraphFrame::Generate {
            state: GenerateState::Repeat {
                input: seed,
                body,
                out_cont: cont,
            },
        });
        Ok(None)
    }

    /// Drives `recurse`: emit the seed, then open its child walk.
    pub(crate) fn eval_recurse(
        &mut self,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        let arity = self.call_arity(node)?;
        let step = RecurseStep {
            filter: if arity == 0 {
                None
            } else {
                Some(self.call_arg(node, 0)?)
            },
            cond: if arity == 2 {
                Some(self.call_arg(node, 1)?)
            } else {
                None
            },
            out_cont: cont,
        };
        let seed = self.materialize_capture(input, resources)?;
        self.recurse_emit(seed, step, resources).map(|()| None)
    }

    /// Publishes one node of a `recurse` walk and defers its children.
    ///
    /// The order is the `def r: ., (f | r); r` definition read literally: the node is
    /// emitted BEFORE its children exist, so the emission is routed now and the
    /// child walk waits in a frame until the emission's consumers are done with
    /// it. The nesting guard is taken here, so the walk's depth is the ledger's
    /// to bound.
    pub(crate) fn recurse_emit(
        &mut self,
        value: Value,
        step: RecurseStep,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        let depth = resources
            .enter_nesting_owned()
            .map_err(|error| EngineRunError::from(EngineError::Resource(error)))?;
        let carried = value.clone();
        self.push_frame(GraphFrame::Generate {
            state: GenerateState::RecurseSeed {
                seed: carried,
                step,
                depth,
            },
        });
        self.pending = Some(GraphTask::Route {
            value: EngineResult::owned(value),
            cont: step.out_cont,
        });
        Ok(())
    }

    /// Opens one `recurse` node's child walk, which is where the three arities
    /// finally differ: `recurse/0` reads the members directly (its `.[]?`
    /// suppresses the scalar mismatch, so a scalar simply has no children)
    /// while `recurse/1` and `recurse/2` run the authored filter, whose errors
    /// propagate.
    pub(crate) fn open_recurse_children(&mut self, seed: Value, step: RecurseStep, depth: OwnedDepthGuard) {
        match step.filter {
            None => {
                self.push_frame(GraphFrame::Generate {
                    state: GenerateState::RecurseMembers {
                        subject: seed,
                        cursor: 0,
                        step,
                        _depth: depth,
                    },
                });
            }
            Some(filter) => {
                self.push_frame(GraphFrame::Generate {
                    state: GenerateState::RecurseChild { step, _depth: depth },
                });
                self.pending = Some(GraphTask::Eval {
                    node: filter,
                    input: EngineResult::owned(seed),
                    cont: self.frames.len(),
                });
            }
        }
    }

    /// Admits one child into the walk, through `recurse/2`'s gate when it has
    /// one.
    pub(crate) fn consume_recurse_child(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let child = self.materialize_capture(value, resources)?;
        let step = match self.frames.as_slice().get(index) {
            Some(GraphFrame::Generate {
                state: GenerateState::RecurseChild { step, .. },
            }) => *step,
            _ => {
                return Err(internal_contract("recurse frame kind changed under capture"));
            }
        };
        self.raise_cont = step.out_cont;
        // The TAIL STEP for `recurse/1` and `recurse/2`: a child filter with
        // nothing left to produce leaves its parent with no children after this
        // one, so the parent's frame — and the nesting level its guard holds —
        // belong to the child now, not stacked under it. A LINEAR chain is
        // therefore iteration and costs O(1) levels, which is why
        // `1 | [recurse(if . < 1000000 then .+1 else empty end)]` runs at all;
        // a filter that can still emit fails the observation and keeps the
        // level, so a genuine tree is bounded exactly as before.
        if self.trim_spent_frames(index.saturating_add(1), resources) {
            self.frames.pop();
        }
        let Some(cond) = step.cond else {
            return self.recurse_emit(child, step, resources).map(|()| None);
        };
        let seed = child.clone();
        self.push_frame(GraphFrame::Generate {
            state: GenerateState::RecurseCond { child, step },
        });
        self.pending = Some(GraphTask::Eval {
            node: cond,
            input: EngineResult::owned(seed),
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Applies `recurse/2`'s gate to one child, exactly as `select` would.
    pub(crate) fn consume_recurse_cond(
        &mut self,
        index: usize,
        value: &EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        if !truth::is_truthy(value).map_err(|_| internal_contract("recurse condition truth check failed"))? {
            return Ok(None);
        }
        let (child, step) = match self.frames.as_slice().get(index) {
            Some(GraphFrame::Generate {
                state: GenerateState::RecurseCond { child, step },
            }) => (child.clone(), *step),
            _ => {
                return Err(internal_contract("recurse frame kind changed under capture"));
            }
        };
        self.recurse_emit(child, step, resources).map(|()| None)
    }

    /// Drives `combinations`: the arity-0 form reads its dimensions straight
    /// off the input, the arity-1 form has to evaluate its count first.
    pub(crate) fn eval_combinations(
        &mut self,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        let subject = self.materialize_capture(input, resources)?;
        if self.call_arity(node)? == 0 {
            return self.open_combinations(subject, cont, resources).map(|()| None);
        }
        let width = self.call_arg(node, 0)?;
        let seed = subject.clone();
        self.push_frame(GraphFrame::Generate {
            state: GenerateState::CombinationsWidth {
                input: subject,
                width: 0,
                out_cont: cont,
            },
        });
        self.pending = Some(GraphTask::Eval {
            node: width,
            input: EngineResult::owned(seed),
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Adds one count output to `combinations(n)`'s running dimension total.
    ///
    /// Each output contributes `range(n)`'s own element count — a negative or
    /// zero `n` contributes nothing, a fractional one rounds up, and a
    /// non-numeric one raises `Range bounds must be numeric` — because the
    /// dimension vector is `[range(n)] | map($dot)` over the whole stream.
    /// Nothing is published here: the odometer opens when the count filter is
    /// exhausted, in [`Self::close_combinations_width`].
    pub(crate) fn consume_combinations_width(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let count = self.materialize_capture(value, resources)?;
        let out_cont = match self.frames.as_slice().get(index) {
            Some(GraphFrame::Generate {
                state: GenerateState::CombinationsWidth { out_cont, .. },
            }) => *out_cont,
            _ => {
                return Err(internal_contract("combinations frame kind changed under capture"));
            }
        };
        self.raise_cont = out_cont;
        let step = generate::range_width(generate::bound(&count, resources)?);
        let Some(GraphFrame::Generate {
            state: GenerateState::CombinationsWidth { width, .. },
        }) = self.frames.as_mut_slice().get_mut(index)
        else {
            return Err(internal_contract("combinations frame kind changed under capture"));
        };
        *width = width.saturating_add(step);
        Ok(None)
    }

    /// Closes `combinations(n)`'s count filter: the accumulated total becomes
    /// that many copies of the retained input, and the odometer opens over them.
    pub(crate) fn close_combinations_width(
        &mut self,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, EngineRunError> {
        let Some(GraphFrame::Generate {
            state: GenerateState::CombinationsWidth { input, width, out_cont },
        }) = self.frames.pop()
        else {
            return Err(internal_contract("combinations frame kind changed under pop"));
        };
        self.raise_cont = out_cont;
        let mut dimensions = Array::try_with_capacity(width).map_err(|_| EngineRunError::allocation_failure())?;
        for _ in 0..width {
            let copy = input.clone();
            dimensions
                .try_push(copy)
                .map_err(|_| EngineRunError::allocation_failure())?;
        }
        self.open_combinations(Value::Array(dimensions), out_cont, resources)?;
        Ok(true)
    }

    /// Opens the odometer, or answers the ONE empty combination a zero-length
    /// subject has (`[] | [combinations]` is `[[]]`, never `[]`).
    pub(crate) fn open_combinations(
        &mut self,
        subject: Value,
        out_cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        match generate::dimensions(&subject, resources)? {
            Dimensions::None => {
                let empty = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
                self.pending = Some(GraphTask::Route {
                    value: EngineResult::owned(Value::Array(empty)),
                    cont: out_cont,
                });
                Ok(())
            }
            Dimensions::Array(width) => {
                self.push_frame(GraphFrame::Generate {
                    state: GenerateState::Combinations {
                        subject,
                        odometer: Odometer::new(width),
                        out_cont,
                    },
                });
                Ok(())
            }
        }
    }

    /// Routes one generator output into the shared consumer walk.
    pub(crate) fn consume_generate(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        match self.frames.as_slice().get(index) {
            Some(GraphFrame::Generate { state }) => {
                Self::cooperate(state, resources)?;
                match state {
                    GenerateState::RangeBound { .. } => self.consume_range_bound(index, value, resources),
                    GenerateState::LoopCond { .. } => self.consume_loop_cond(index, &value),
                    GenerateState::LoopUpdate { .. } => self.consume_loop_update(index, value, resources),
                    GenerateState::RecurseChild { .. } => self.consume_recurse_child(index, value, resources),
                    GenerateState::RecurseCond { .. } => self.consume_recurse_cond(index, &value, resources),
                    GenerateState::CombinationsWidth { .. } => self.consume_combinations_width(index, value, resources),
                    GenerateState::Range { .. }
                    | GenerateState::ValueRange { .. }
                    | GenerateState::LoopStep { .. }
                    | GenerateState::Repeat { .. }
                    | GenerateState::RecurseSeed { .. }
                    | GenerateState::RecurseMembers { .. }
                    | GenerateState::Combinations { .. } => Err(internal_contract(
                        "a producing generator state reached the consumer walk",
                    )),
                }
            }
            _ => Err(internal_contract("generator frame kind changed under capture")),
        }
    }

    /// Gives the ledger its turn before a generator resumes.
    ///
    /// A generator resumes in exactly two ways — a consumer state receives one
    /// argument value, or a producer state reaches the top of the stack — so
    /// this is called from both walks and from nowhere else. Whether it does
    /// anything is the table's answer, not the caller's: `while`, `until` and
    /// `repeat` have no bound of their own, and this is where a cancellation or
    /// a deadline gets to stop them.
    pub(crate) fn cooperate(
        state: &GenerateState<'source>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        if !state.cooperative() {
            return Ok(());
        }
        resources
            .check_control()
            .map_err(|error| EngineRunError::from(EngineError::Control(error)))
    }

    /// Resumes the generator at the top of the stack.
    pub(crate) fn advance_generate(&mut self, resources: &mut ResourceContext<'_>) -> Result<bool, EngineRunError> {
        let top = self
            .frames
            .as_slice()
            .len()
            .checked_sub(1)
            .ok_or_else(|| internal_contract("an empty stack reached the generator advance"))?;
        match self.frames.as_slice().get(top) {
            // The one ACCUMULATING consumer: exhaustion is when its answer is
            // finally known, so it publishes on the way out instead of popping.
            Some(GraphFrame::Generate {
                state: GenerateState::CombinationsWidth { .. },
            }) => self.close_combinations_width(resources),
            Some(GraphFrame::Generate { state }) if state.consumes() => {
                // A consumer at the top means its argument generator is
                // exhausted: pop it, exactly as `SelectBody` and the loop
                // consumers do.
                Self::cooperate(state, resources)?;
                self.frames.pop();
                Ok(true)
            }
            Some(GraphFrame::Generate { state }) => {
                Self::cooperate(state, resources)?;
                self.resume_generate(top, resources)
            }
            _ => Err(internal_contract("generator frame kind changed under advance")),
        }
    }

    /// Resumes the producer state at the top of the stack.
    pub(crate) fn resume_generate(
        &mut self,
        top: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, EngineRunError> {
        match self.frames.as_slice().get(top) {
            Some(GraphFrame::Generate {
                state: GenerateState::Range { .. },
            }) => self.advance_range(top),
            Some(GraphFrame::Generate {
                state: GenerateState::ValueRange { .. },
            }) => self.advance_value_range(top, resources),
            Some(GraphFrame::Generate {
                state: GenerateState::LoopStep { .. },
            }) => self.advance_loop_step(),
            Some(GraphFrame::Generate {
                state: GenerateState::Repeat { .. },
            }) => self.advance_repeat(top),
            Some(GraphFrame::Generate {
                state: GenerateState::RecurseSeed { .. },
            }) => self.advance_recurse_seed(),
            Some(GraphFrame::Generate {
                state: GenerateState::RecurseMembers { .. },
            }) => self.advance_recurse_members(top, resources),
            Some(GraphFrame::Generate {
                state: GenerateState::Combinations { .. },
            }) => self.advance_combinations(top, resources),
            _ => Err(internal_contract("generator frame kind changed under advance")),
        }
    }

    /// Steps the `range` cursor, IN PLACE: a re-push would need a resource
    /// context this transition does not have — the same reason
    /// [`GraphFrame::PathEmit`] mutates rather than re-pushes.
    pub(crate) fn advance_range(&mut self, top: usize) -> Result<bool, EngineRunError> {
        let Some(GraphFrame::Generate {
            state:
                GenerateState::Range {
                    cursor,
                    first,
                    out_cont,
                },
        }) = self.frames.as_mut_slice().get_mut(top)
        else {
            return Err(internal_contract("range frame kind changed under advance"));
        };
        let out_cont = *out_cont;
        let Some(next) = cursor.next() else {
            self.frames.pop();
            return Ok(true);
        };
        // The cursor's advance test gates the FIRST value too
        // (`range(-0.0; 0)` emits nothing): the authored `from` number
        // replaces the accumulator's rendering only when the walk actually
        // starts, so the literal's spelling survives the first position
        // exactly as the reference emits it.
        let value = match first.take() {
            Some(from) => EngineResult::owned(Value::Number(from)),
            None => EngineResult::owned(generate::emit(next)?),
        };
        self.pending = Some(GraphTask::Route { value, cont: out_cont });
        Ok(true)
    }

    /// Steps the `range/3` VALUE cursor, in place for the same reason
    /// [`Self::advance_range`] does.
    ///
    /// The step ITSELF may raise — `. + $by` is an ordinary addition — so
    /// unlike the `f64` cursor this one can end a frame with an error rather
    /// than with exhaustion. The frame is left in place on that path: the raise
    /// unwinds it exactly as any other generator error does.
    pub(crate) fn advance_value_range(
        &mut self,
        top: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, EngineRunError> {
        let Some(GraphFrame::Generate {
            state: GenerateState::ValueRange { cursor, out_cont },
        }) = self.frames.as_mut_slice().get_mut(top)
        else {
            return Err(internal_contract("range frame kind changed under advance"));
        };
        let out_cont = *out_cont;
        let Some(next) = cursor.next(resources)? else {
            self.frames.pop();
            return Ok(true);
        };
        self.pending = Some(GraphTask::Route {
            value: EngineResult::owned(next),
            cont: out_cont,
        });
        Ok(true)
    }

    /// Runs `while`'s deferred step: the emission is done with, so the update
    /// may finally run over the value that produced it.
    pub(crate) fn advance_loop_step(&mut self) -> Result<bool, EngineRunError> {
        let Some(GraphFrame::Generate {
            state:
                GenerateState::LoopStep {
                    current,
                    cond,
                    update,
                    out_cont,
                    cond_index,
                },
        }) = self.frames.pop()
        else {
            return Err(internal_contract("loop frame kind changed under pop"));
        };
        self.push_frame(GraphFrame::Generate {
            state: GenerateState::LoopUpdate {
                form: LoopForm::While,
                cond,
                update,
                out_cont,
                cond_index,
            },
        });
        self.pending = Some(GraphTask::Eval {
            node: update,
            input: EngineResult::owned(current),
            cont: self.frames.len(),
        });
        Ok(true)
    }

    /// Runs one more `repeat` round over the ORIGINAL input.
    ///
    /// The frame never pops: `repeat` is non-terminating by design, and the
    /// round's outputs route through the generator's OWN out-continuation, so
    /// this frame is transparent to them.
    pub(crate) fn advance_repeat(&mut self, top: usize) -> Result<bool, EngineRunError> {
        let Some(GraphFrame::Generate {
            state: GenerateState::Repeat { input, body, out_cont },
        }) = self.frames.as_slice().get(top)
        else {
            return Err(internal_contract("repeat frame kind changed under advance"));
        };
        let round = input.clone();
        let (body, out_cont) = (*body, *out_cont);
        self.pending = Some(GraphTask::Eval {
            node: body,
            input: EngineResult::owned(round),
            cont: out_cont,
        });
        Ok(true)
    }

    /// Opens the child walk of a `recurse` node whose value has been emitted.
    pub(crate) fn advance_recurse_seed(&mut self) -> Result<bool, EngineRunError> {
        let Some(GraphFrame::Generate {
            state: GenerateState::RecurseSeed { seed, step, depth },
        }) = self.frames.pop()
        else {
            return Err(internal_contract("recurse frame kind changed under pop"));
        };
        self.raise_cont = step.out_cont;
        self.open_recurse_children(seed, step, depth);
        Ok(true)
    }

    /// Recurses into `recurse/0`'s next member, or pops when they run out.
    pub(crate) fn advance_recurse_members(
        &mut self,
        top: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, EngineRunError> {
        let child = {
            let Some(GraphFrame::Generate {
                state: GenerateState::RecurseMembers {
                    subject, cursor, step, ..
                },
            }) = self.frames.as_mut_slice().get_mut(top)
            else {
                return Err(internal_contract("recurse frame kind changed under advance"));
            };
            let step = *step;
            match member_at(subject, *cursor) {
                None => None,
                Some(child) => {
                    *cursor += 1;
                    let last = member_at(subject, *cursor).is_none();
                    Some((child.clone(), step, last))
                }
            }
        };
        let Some((child, step, last)) = child else {
            self.frames.pop();
            return Ok(true);
        };
        // The register's member write: the child's component (`0`, `"key"`)
        // extends the register — `recurse/0` IS `..`, so its walk tracks.
        // Read BEFORE the tail pop (the frame vanishes for the last member).
        let component = if self.path.is_some() {
            self.recurse_member_component(top, resources)?
        } else {
            None
        };
        // `recurse/0`'s TAIL STEP, the member-cursor twin of the child-filter
        // one in [`Self::consume_recurse_child`]: the LAST member leaves this
        // frame with nothing to do but pop, so it pops NOW and hands the child
        // its nesting level instead of nesting under it. A single-child chain —
        // `[[[[…]]]]` under `..` — is iteration and costs O(1) levels; a
        // container with members still to walk keeps its frame and its level,
        // so a branching document is bounded exactly as before.
        if last {
            self.frames.pop();
        }
        if let Some(component) = component {
            let child_result = EngineResult::owned(child.clone());
            self.path_each_child(&child_result, Some(component), step.out_cont, resources)?;
        }
        self.recurse_emit(child, step, resources)?;
        Ok(true)
    }

    /// The path component one `recurse` member contributes (an array's
    /// position, an object's member key).
    pub(crate) fn recurse_member_component(
        &self,
        top: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<jqf_builtins::semantics::path::PathStep>, EngineRunError> {
        let Some(GraphFrame::Generate {
            state: GenerateState::RecurseMembers { subject, cursor, .. },
        }) = self.frames.as_slice().get(top)
        else {
            return Ok(None);
        };
        let index = *cursor - 1;
        match subject.untagged() {
            Value::Array(_) => Ok(Some(jqf_builtins::semantics::path::PathStep::Index(
                i64::try_from(index).map_err(|_| EngineRunError::allocation_failure())?,
            ))),
            Value::Object(object) => {
                let Some(key) = object.get_index(index) else {
                    return Ok(None);
                };
                Ok(Some(jqf_builtins::semantics::path::PathStep::Key(
                    jqf_builtins::semantics::path::charged_key(key.key(), resources)?,
                )))
            }
            _ => Ok(None),
        }
    }

    /// Emits the odometer's next tuple, or pops when it is spent.
    pub(crate) fn advance_combinations(
        &mut self,
        top: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, EngineRunError> {
        let (tuple, out_cont) = {
            let Some(GraphFrame::Generate {
                state:
                    GenerateState::Combinations {
                        subject,
                        odometer,
                        out_cont,
                    },
            }) = self.frames.as_mut_slice().get_mut(top)
            else {
                return Err(internal_contract("combinations frame kind changed under advance"));
            };
            let out_cont = *out_cont;
            let Value::Array(dimensions) = subject.untagged() else {
                return Err(internal_contract("a combinations subject that is not an array"));
            };
            (odometer.next(dimensions, resources)?, out_cont)
        };
        let Some(tuple) = tuple else {
            self.frames.pop();
            return Ok(true);
        };
        self.pending = Some(GraphTask::Route {
            value: EngineResult::owned(tuple),
            cont: out_cont,
        });
        Ok(true)
    }
}
