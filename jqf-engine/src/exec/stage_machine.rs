//! Single-slot fast-path executor for a bare-`Stage` residual.
//!
//! Extracted from [`super::frames`]: this machine never engages graph
//! dispatch. [`super::GraphMachine`] stays in `frames`. Shared poll
//! vocabulary ([`super::RunPoll`], [`super::RunStep`], [`super::StepOutcome`])
//! remains there.

#[allow(clippy::wildcard_imports)]
use super::*;

/// One frame of an active fan-out: the container and the next child position to
/// emit. A located container retains borrowed document authority (each child a
/// fresh located product); an owned container (from a literal or constructor
/// base, e.g. `[1,2,3][]`) clones its children out.
///
/// The RESUME-AT index is frame state, not a hard-coded `step + 1`: a `.[]`
/// fan-out resumes its children AFTER the iterating step, while a `..` fan-out
/// resumes them AT the descend step itself, which is exactly what makes the
/// descent recursive.
pub(crate) struct Frame<'source> {
    /// The residual step index this frame's children resume at.
    pub(crate) next_at: usize,
    /// The container being iterated (located or owned).
    pub(crate) container: Container<'source>,
    /// The next child position to emit.
    pub(crate) cursor: usize,
}

/// The single-slot pending work: descend `steps[at..]` over one value.
pub(crate) struct PendingDescend<'source> {
    pub(crate) value: EngineResult<'source>,
    pub(crate) at: usize,
}

/// One innermost-frame advance, returned with its borrow of the frame stack
/// already released so the driver may re-seed or tear down without a conflict.
pub(crate) enum FrameAdvance<'source> {
    /// The frame yielded a child; descend it from `at`.
    Descend { value: EngineResult<'source>, at: usize },
    /// A frame was exhausted and popped; keep advancing.
    Continue,
    /// No frames remain; the stream is complete.
    Complete,
    /// Materializing the child failed (an internal navigation contract).
    Fail(EngineRunError),
}

/// The single-slot fast-path executor driving one bare-`Stage` residual to an
/// ordered result stream.
///
/// `steps` is borrowed from the compiled program (a program lifetime; no
/// per-frame or per-value step copies), and `base` is the pushdown prefix length
/// — the global step offset the residual reports mismatches against. The current
/// task lives in `pending` (a single slot); `frames` is the lazily allocated
/// `.[]` frame stack. State is entirely per-run and self-contained,
/// so running several executors shares no mutable state. A `Choice`/`FlatMap`
/// residual is driven by [`GraphMachine`] instead; this machine engages no graph
/// dispatch, preserving the landed fast path structurally.
pub(crate) struct StageMachine<'program, 'source> {
    pub(crate) steps: &'program [StageStep],
    pub(crate) base: usize,
    pub(crate) pending: Option<PendingDescend<'source>>,
    pub(crate) frames: Option<Vec<Frame<'source>>>,
    /// The run's reusable materializer workspace — the fast path's half of
    /// [`StepScratch`]. Only a `Slice` step over a LOCATED container ever
    /// touches it, so a program without one allocates nothing here.
    pub(crate) workspace: Option<MaterializeWorkspace>,
    pub(crate) armed_error: Option<EngineError>,
    pub(crate) done: bool,
    /// Cumulative frame-task transitions admitted this run. Counted beside
    /// every cooperative work admission when the run carries an iteration
    /// ceiling (`--max-iterations`); untouched (and free) otherwise.
    pub(crate) iterations_done: u64,
    /// The run's opt-in transition ceiling; `None` is unlimited (the default).
    pub(crate) iteration_cap: Option<u64>,
}

impl core::fmt::Debug for StageMachine<'_, '_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Deliberately shallow: the profiler must see no per-frame/per-value
        // formatting on the hot path, so this reports shape, not contents.
        formatter
            .debug_struct("StageMachine")
            .field("residual_steps", &self.steps.len())
            .field("base", &self.base)
            .field("pending", &self.pending.is_some())
            .field("frames", &self.frames.as_ref().map_or(0, Vec::len))
            .field("done", &self.done)
            .finish()
    }
}

impl<'program, 'source> StageMachine<'program, 'source> {
    /// Seeds a fast-path executor whose residual is `steps` (global base `base`)
    /// over one input value. Construction fills the single pending slot and
    /// allocates nothing; the frame stack materializes on the first `.[]` descent.
    pub(crate) fn seed(steps: &'program [StageStep], base: usize, input: EngineResult<'source>) -> Self {
        Self {
            steps,
            base,
            pending: Some(PendingDescend { value: input, at: 0 }),
            frames: None,
            workspace: None,
            armed_error: None,
            done: false,
            iterations_done: 0,
            iteration_cap: None,
        }
    }

    /// Advances the stream by at most one ordered item, charging one work
    /// transition per advance and honoring cooperative pauses.
    ///
    /// # Errors
    ///
    /// Returns [`EngineRunError`]: a typed path/iterate mismatch discovered
    /// mid-iteration, or a machine failure (control, ledger, internal contract).
    pub(crate) fn poll(&mut self, resources: &mut ResourceContext<'_>) -> Result<RunPoll<'source>, EngineRunError> {
        match self.resume(resources)? {
            RunStep::Emit(item) => Ok(RunPoll::Item(item)),
            RunStep::Pending => Ok(RunPoll::Pending),
            RunStep::Complete => Ok(RunPoll::Complete),
        }
    }

    /// Drives the continuation state to the next ordered item, charging one work
    /// transition. Between items it does bounded frame bookkeeping (descend one
    /// child, push/pop one frame); it returns as soon as it emits, pauses, or
    /// completes.
    pub(crate) fn resume(&mut self, resources: &mut ResourceContext<'_>) -> Result<RunStep<'source>, EngineRunError> {
        // Surface an armed trailing failure only after its prefix item was
        // already emitted on the previous advance.
        if let Some(error) = self.armed_error.take() {
            return Err(EngineRunError::from(error));
        }
        if self.done {
            return Ok(RunStep::Complete);
        }
        match resources
            .admit_work_transition()
            .map_err(|error| EngineRunError::from(EngineError::Control(error)))?
        {
            WorkAdmission::Pending => return Ok(RunStep::Pending),
            WorkAdmission::Granted(_) => {}
        }
        admit_iteration(&mut self.iterations_done, self.iteration_cap)?;
        self.advance(resources)
    }

    /// Advances the continuation to the next emit or completion after a work
    /// transition has been admitted. An error here tears the run down: `done` is
    /// set so no outer cursor survives an inner abort.
    pub(crate) fn advance(&mut self, resources: &mut ResourceContext<'_>) -> Result<RunStep<'source>, EngineRunError> {
        loop {
            if let Some(PendingDescend { value, at }) = self.pending.take() {
                // The fast path drives a bare-`Stage` root, which can never carry
                // a `.[$x]` step or a `$var` slice bound (a bound variable needs
                // an enclosing binder, and a binder root engages the graph
                // machine), so its env is empty.
                let descended = {
                    let mut scratch = StepScratch::new(&mut self.workspace, resources);
                    descend(self.steps, self.base, &[], value, at, &mut scratch)
                };
                match descended {
                    Ok(Descended::Leaf(item)) => {
                        // A leaf with no remaining fan-out is the last output.
                        // Finish here so the next poll observes completion
                        // without admitting work just to walk an empty stack.
                        if self.frames.as_ref().is_none_or(Vec::is_empty) {
                            self.done = true;
                        }
                        return Ok(self.absorb(StepOutcome::Emit(item)));
                    }
                    Ok(Descended::FanOut { next_at, container }) => {
                        self.push_frame(next_at, container);
                    }
                    // `..` over a container: SELF FIRST. The frame's children
                    // resume AT the descend step (recursing) while the pending
                    // slot carries the self-emission past it — and the pending
                    // slot is drained before any frame advances, which is what
                    // makes the walk pre-order.
                    Ok(Descended::Descend {
                        next_at,
                        container,
                        value,
                        at,
                    }) => {
                        self.push_frame(next_at, container);
                        self.pending = Some(PendingDescend { value, at });
                        continue;
                    }
                    Ok(Descended::Skip) => {}
                    Err(error) => {
                        self.tear_down();
                        return Err(error);
                    }
                }
            }
            match self.advance_frame(resources) {
                FrameAdvance::Complete => {
                    self.done = true;
                    return Ok(RunStep::Complete);
                }
                FrameAdvance::Continue => {}
                FrameAdvance::Descend { value, at } => {
                    self.pending = Some(PendingDescend { value, at });
                }
                FrameAdvance::Fail(error) => {
                    self.tear_down();
                    return Err(error);
                }
            }
        }
    }

    /// Advances the innermost `.[]` frame by one child: seeds a descent for the
    /// next child, pops an exhausted frame, or signals completion. Its borrow of
    /// the frame stack ends with the returned value, so the caller may tear the
    /// run down without a conflicting borrow.
    pub(crate) fn advance_frame(&mut self, resources: &mut ResourceContext<'_>) -> FrameAdvance<'source> {
        let Some(frames) = self.frames.as_mut() else {
            return FrameAdvance::Complete;
        };
        let Some(frame) = frames.as_mut_slice().last_mut() else {
            return FrameAdvance::Complete;
        };
        let mut scratch = StepScratch::new(&mut self.workspace, resources);
        match next_child(&frame.container, frame.cursor, &mut scratch) {
            Ok(Some(child)) => {
                let at = frame.next_at;
                frame.cursor += 1;
                FrameAdvance::Descend { value: child, at }
            }
            Ok(None) => {
                frames.pop();
                FrameAdvance::Continue
            }
            Err(error) => FrameAdvance::Fail(error),
        }
    }

    /// Pushes one fan-out frame (a `.[]` iteration or a `..` descent level),
    /// allocating the frame stack on the first descent.
    pub(crate) fn push_frame(&mut self, next_at: usize, container: Container<'source>) {
        let frames = self.frames.get_or_insert_with(Vec::new);
        frames.push(Frame {
            next_at,
            container,
            cursor: 0,
        });
    }

    /// Discards the entire frame stack and marks the run done: an unsuppressed
    /// failure leaves no resumable cursor behind.
    pub(crate) fn tear_down(&mut self) {
        self.done = true;
        self.pending = None;
        if let Some(frames) = self.frames.as_mut() {
            frames.clear();
        }
    }

    /// Buffers a step's single output and arms any trailing failure, preserving
    /// the emitted prefix: the item is returned now, the failure on the next
    /// advance. The fan-out machine only ever passes [`StepOutcome::Emit`]; the
    /// [`StepOutcome::EmitThenFail`] path is the producerless test arm.
    #[cfg_attr(
        not(test),
        allow(
            clippy::unused_self,
            reason = "the only `self` read is the cfg(test) EmitThenFail arm"
        )
    )]
    pub(crate) fn absorb(&mut self, outcome: StepOutcome<'source>) -> RunStep<'source> {
        match outcome {
            StepOutcome::Emit(item) => RunStep::Emit(item),
            #[cfg(test)]
            StepOutcome::EmitThenFail { item, error } => {
                self.armed_error = Some(error);
                RunStep::Emit(item)
            }
        }
    }
}
