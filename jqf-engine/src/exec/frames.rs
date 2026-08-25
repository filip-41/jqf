//! Frame, task, and machine types for the iterative executor.
//!
//! One job: own the stack vocabulary ([`GraphFrame`], [`GraphTask`],
//! [`StageMachine`], [`GraphMachine`]) and the free helpers every sibling
//! evaluator calls. Graph-machine methods live in `eval`, `fold`,
//! `dispatch`, `pathmode`, and `route` so this file stays types-and-helpers.

#[allow(clippy::wildcard_imports)]
use super::*;

pub(crate) fn input_source<'request>(resources: &'request ResourceContext<'_>) -> Option<&'request dyn InputSource> {
    resources
        .host_extension()?
        .downcast_ref::<InputSourceHandle>()
        .map(|handle| handle as &dyn InputSource)
}

/// The standing recursive-definition call ceiling.
///
/// The reference recurses on its native stack without a bound; jqf evaluates
/// callable bodies through nested owned runs, so an unbounded recursion would
/// grow the native stack instead. The ceiling is a safe depth, and the refusal
/// is a machine error (catalogued divergence, like the inlining bound it
/// replaces).
///
/// What the ceiling bounds is GENUINE nesting — a self-call whose
/// continuation the trampoline cannot flatten. A self-call in a marked
/// position (a comma's last member, a pipe's right member, a pipe's left
/// member, a conditional arm, a binding/label/try body) is TRAMPOLINED
/// instead: the body re-evaluates at the call's own routing position and the
/// callable depth stays flat, so `limit(5000; f)` over `def f: 1, f` never
/// approaches the ceiling. Frame-stack memory is still O(depth): a
/// trampolined `def f($n): if $n==0 then 0 else f($n-1) end; f(5000)` holds
/// 5001 live frames (296 B each). The ceiling bounds nested callable
/// machines, not frame-vector residency. The ceiling is therefore NOT a
/// bound on how many outputs a lazy consumer can pull; it bounds only the
/// depth of continuations that must run after the call returns (a self-call
/// inside a collection, an object member, a `+`/`and`/`//` operand, …),
/// where the reference's own native stack is the comparable — and equally
/// finite — resource.
///
/// The ceiling was raised from 512 to 1 000 when 512 refused ordinary
/// recursive numeric and tree code (`600|fact`) that the reference runs. The
/// depth sits below the cooperative boundary where a nested-callable
/// Pending-resume path loses progress (a recursion needing a second
/// cooperative entry re-descends from the top and can never deepen past the
/// boundary), clears the request thread's 256 MiB stack with orders of
/// magnitude to spare, and answers `600|fact`. `10000 | def f: if .>0 then
/// foreach . as $x (.;.-1;f) end; f` stays refused at this ceiling: its
/// 10 000 nested levels cross the cooperative boundary by construction, and
/// closing it is the Pending-resume fix, not a constant.
pub(crate) const CALLABLE_DEPTH_LIMIT: usize = 1_000;

/// The trampolined (flat) recursion round ceiling.
///
/// The tail trampoline re-evaluates the callable body without growing the
/// callable depth, so a genuinely divergent self-call — `def f: f; f` — would
/// otherwise loop forever (the compile-time always-recurses refusal was
/// dropped, which the reference's own lazy evaluation never reaches for an
/// unreached call). This bound counts TRAMPOLINE ROUNDS and raises the
/// same typed depth error once a recursion has run past it without the
/// consumer cutting it. 1 000 000 is far above any legitimate depth a lazy
/// consumer can demand (`limit(5000; f)` in the trampoline pins is 5 000) and low
/// enough that a divergent program terminates promptly.
pub(crate) const TAIL_ROUND_LIMIT: u64 = 1_000_000;

/// The machine error a recursive call past [`CALLABLE_DEPTH_LIMIT`] raises.
pub(crate) fn callable_depth_error() -> EngineRunError {
    EngineRunError::Codec(CodecError::new(CodecFailureKind::Resource(
        ResourceError::RecursionLimit {
            limit: CALLABLE_DEPTH_LIMIT as u64,
        },
    )))
}

/// The machine error a trampolined recursion past [`TAIL_ROUND_LIMIT`] raises.
pub(crate) fn tail_round_error() -> EngineRunError {
    EngineRunError::Codec(CodecError::new(CodecFailureKind::Resource(
        ResourceError::RecursionLimit {
            limit: TAIL_ROUND_LIMIT,
        },
    )))
}

/// The machine error a run past its opt-in iteration ceiling raises.
///
/// The ceiling is a host dial (`--max-iterations`) carried on the run itself
/// and enforced beside every cooperative work admission; `None` (the default)
/// never refuses. Like the depth limits it is a resource-family machine
/// error: not catch-eligible, exit class 5. The prose rides
/// [`ResourceError::HostFailure`] verbatim because no frozen `ResourceLimit`
/// dimension names a transition count, and the sentence must name the flag
/// that set it.
pub(crate) fn iteration_ceiling_error() -> EngineRunError {
    EngineRunError::Codec(CodecError::new(CodecFailureKind::Resource(
        ResourceError::HostFailure {
            detail: "--max-iterations frame-transition ceiling exceeded",
        },
    )))
}

/// Admits one frame-task transition against the run's optional iteration
/// ceiling (`--max-iterations`). Called right after each cooperative work
/// admission in both machines' drive loops — the existing checkpoint where a
/// refusal can tear the run down exactly like any other machine failure.
#[inline]
pub(crate) fn admit_iteration(done: &mut u64, cap: Option<u64>) -> Result<(), EngineRunError> {
    if let Some(cap) = cap {
        *done = done.saturating_add(1);
        if *done > cap {
            return Err(iteration_ceiling_error());
        }
    }
    Ok(())
}

/// Records one raise's diagnostic record — the ONE copy shared by the graph
/// machine and path mode.
pub(crate) fn emit_raise_diagnostic(resources: &ResourceContext<'_>, error: &EngineRunError, caught: Option<u32>) {
    use jqf_resource::diag::DiagnosticRecord;
    let mut record = DiagnosticRecord::new(error.diagnostic_code());
    record.step_index = error.diagnostic_step_index();
    record.kind = error.diagnostic_operand_kind().map(crate::exec::message::kind_name);
    record.operand = error.diagnostic_operand();
    record.caught = caught;
    resources.record_diagnostic(record);
}

/// Which `(from, upto, by)` slot the authored argument at `depth` fills.
///
/// `range(upto)` is the only arity that skips a slot: its single argument is
/// the UPPER bound, not the lower one, so it fills slot 1 while every other
/// arity fills slots in order.
pub(crate) const fn range_slot(arity: usize, depth: usize) -> usize {
    if arity == 1 { 1 } else { depth }
}

/// One member of an owned container in `.[]` order, or `None` for a scalar or
/// an exhausted cursor.
///
/// A scalar answering `None` rather than an error IS `recurse/0`'s `.[]?`: the
/// suppression is structural here, so no error is ever constructed to be thrown
/// away.
pub(crate) fn member_at(subject: &Value, cursor: usize) -> Option<&Value> {
    match subject.untagged() {
        Value::Array(array) => array.get(cursor),
        Value::Object(object) => object.get_index(cursor).map(jqf_data::ObjectEntry::value),
        _ => None,
    }
}

/// The object key a `break`'s target label rides under.
///
/// This is the observable spelling, not an internal choice: `label $o | try
/// (break $o) catch .` renders `{"__jq":0}`, and `error({"__jq":0})` IS a
/// break. The marker is therefore a REACHABLE
/// user value, and reproducing the exact key is a parity requirement.
pub(crate) const BREAK_MARKER_KEY: &str = "__jq";

/// Builds the break marker `{"__jq": slot}` a `break $out` raises.
pub(crate) fn break_marker(slot: LabelSlot, _resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let mut builder = ObjectBuilder::try_with_capacity(1).map_err(|_| EngineRunError::allocation_failure())?;
    builder
        .try_insert_last(
            jqf_data::ObjectKey::try_from_str(BREAK_MARKER_KEY).map_err(|_| EngineRunError::allocation_failure())?,
            Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(i64::from(slot)))),
        )
        .map_err(|_| EngineRunError::allocation_failure())?;
    let object = builder.try_finish().map_err(|_| EngineRunError::allocation_failure())?;
    Ok(Value::Object(object))
}

/// Reads a raised value back as a break marker, or `None` when it is an ordinary
/// error value.
///
/// Exactly one member, keyed [`BREAK_MARKER_KEY`], whose value is numerically a
/// non-negative whole number in slot range. A value that merely resembles a
/// marker but names no live label walks past every barrier and surfaces as an
/// ordinary error, which is the observable behaviour (`{"__jq":5}` with no
/// matching label
/// renders `(not a string): {"__jq":5}`).
///
/// The test is NUMERIC, not representational: the payload is compared as a
/// number, so `{"__jq": 0.0}` breaks label 0 exactly as `{"__jq": 0}` does —
/// reading it as an integer SPELLING made the two diverge. A non-integral,
/// negative, or out-of-range magnitude matches no slot and is left to surface
/// as an ordinary error.
pub(crate) fn marker_slot(value: &Value) -> Option<LabelSlot> {
    let Value::Object(object) = value else {
        return None;
    };
    if object.len() != 1 {
        return None;
    }
    let Value::Number(number) = object.get(BREAK_MARKER_KEY)? else {
        return None;
    };
    // `floor_binary64` rather than `f64::fract`: this crate is `no_std`, and the
    // slice-bound normalizer already uses it as the integrality test.
    let raw = jqf_builtins::semantics::order::to_f64(number);
    #[allow(
        clippy::float_cmp,
        reason = "an exact equality against the floor IS the integrality test; a \
                  tolerance would admit non-integral markers"
    )]
    if !raw.is_finite()
        || raw < 0.0
        || raw > f64::from(LabelSlot::MAX)
        || jqf_builtins::semantics::arith::floor_binary64(raw) != raw
    {
        return None;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the guard above bounds the value to a whole non-negative LabelSlot"
    )]
    Some(raw as LabelSlot)
}

/// What a countdown does with the item it has just paid for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CountedVerdict {
    /// Drop it: the count is not yet spent (`skip`/`nth` before the index).
    Withhold,
    /// Publish it and keep the source running.
    Publish,
    /// Publish it and cut the source: no later item can change the answer.
    PublishAndCut,
}

/// The `1` a countdown pays per item, and the `0` it compares against — both
/// spelled as the arena's own literals so the arithmetic and the comparison are
/// bit-for-bit the ones the `foreach` shape performed.
pub(crate) fn counted_unit() -> Value {
    Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(1)))
}

/// The countdown's comparison target.
pub(crate) fn counted_zero() -> Value {
    Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(0)))
}

/// One unit of the general countdown: `. - 1` plus the two sign answers the
/// three counted laws read off the new state.
pub(crate) struct PaidCount {
    /// The new state.
    state: Value,
    /// `. <= 0` — `limit`'s stop test.
    exhausted: bool,
    /// `. < 0` — `skip`/`nth`'s "the count is spent" test.
    spent: bool,
}

/// Pays one unit of a general (non-integer-counter) countdown.
///
/// Every operation here is the operator the `foreach` spelling used, called
/// through the same entry point: the subtraction is `Binary{Subtract}`'s, and
/// the two tests are `Binary{LessEqual}`'s and `Binary{Less}`'s against the same
/// integer zero. A comparison cannot fail for a number (only container recursion
/// reaches the depth cap, and a successful subtraction leaves a number), so its
/// failure is folded into the subtraction's — the caller renders the
/// subtraction's operands either way.
pub(crate) fn spend_one(
    state: &Value,
    resources: &ResourceContext<'_>,
) -> Result<PaidCount, jqf_builtins::semantics::binary::BinaryError> {
    let next = binary::apply(BinaryKind::Subtract, state, &counted_unit(), resources)?;
    let zero = counted_zero();
    let exhausted = matches!(
        binary::apply(BinaryKind::LessEqual, &next, &zero, resources)?,
        Value::Bool(true)
    );
    let spent = matches!(
        binary::apply(BinaryKind::Less, &next, &zero, resources)?,
        Value::Bool(true)
    );
    Ok(PaidCount {
        state: next,
        exhausted,
        spent,
    })
}

/// Pays one unit of a countdown and answers what to do with the item that unit
/// bought.
///
/// The two counted drives — the frame machine's and path mode's — share this
/// one function, which is what makes them one law rather than two transcriptions
/// of it.
pub(crate) fn counted_verdict(
    kind: CountedKind,
    state: &mut CountedState,
    resources: &ResourceContext<'_>,
) -> Result<CountedVerdict, EngineRunError> {
    match state {
        CountedState::Counter(remaining) => Ok(counter_step(kind, remaining)),
        CountedState::Exact(carried) => match spend_one(carried, resources) {
            Ok(paid) => {
                let verdict = exact_step(kind, &paid);
                *carried = paid.state;
                Ok(verdict)
            }
            Err(error) => {
                let failure = ArithFailure::from_binary(error, resources)?;
                Err(EngineRunError::Arithmetic {
                    failure,
                    left: message::dump_trunc_owned(carried)?,
                    right: message::dump_trunc_owned(&counted_unit())?,
                })
            }
        },
    }
}

/// The verdict a general countdown's paid unit produces.
pub(crate) const fn exact_step(kind: CountedKind, paid: &PaidCount) -> CountedVerdict {
    match kind {
        // `$item, if . <= 0 then break $out else empty end`: the item is
        // published either way, and the test only decides whether the source
        // survives it.
        CountedKind::Limit => {
            if paid.exhausted {
                CountedVerdict::PublishAndCut
            } else {
                CountedVerdict::Publish
            }
        }
        // `if . < 0 then $item else empty end`.
        CountedKind::Skip => {
            if paid.spent {
                CountedVerdict::Publish
            } else {
                CountedVerdict::Withhold
            }
        }
        // `skip`'s test with `first(…)`'s cap folded in.
        CountedKind::Nth => {
            if paid.spent {
                CountedVerdict::PublishAndCut
            } else {
                CountedVerdict::Withhold
            }
        }
    }
}

/// The verdict an exact-integer countdown's paid unit produces, and the cursor
/// step that pays it.
///
/// `remaining` counts what the state's SIGN would otherwise have to be
/// recomputed for: for `limit` it is how many items may still be published, for
/// `skip`/`nth` how many are still owed to the count before the first
/// publication. Saturation is the unbounded case and needs no arm — a count of
/// [`u64::MAX`] can never be paid down by a real source.
pub(crate) fn counter_step(kind: CountedKind, remaining: &mut u64) -> CountedVerdict {
    match kind {
        CountedKind::Limit => {
            *remaining = remaining.saturating_sub(1);
            if *remaining == 0 {
                CountedVerdict::PublishAndCut
            } else {
                CountedVerdict::Publish
            }
        }
        CountedKind::Skip => {
            if *remaining > 0 {
                *remaining -= 1;
                CountedVerdict::Withhold
            } else {
                CountedVerdict::Publish
            }
        }
        CountedKind::Nth => {
            if *remaining > 0 {
                *remaining -= 1;
                CountedVerdict::Withhold
            } else {
                CountedVerdict::PublishAndCut
            }
        }
    }
}

/// Classifies a bound count into the countdown state that reproduces it.
///
/// The fast row is exactly the counts whose `n - k` countdown is the integer
/// countdown: an exact INTEGER (whose `ceil` and `floor` are both itself), and a
/// FLOAT that is integral and non-negative (whose repeated `- 1.0` is exact
/// below 2^53 and, above it, makes no progress at all — which is the same
/// observable as the saturated counter, since no source emits 2^53 items).
///
/// Everything else declines to [`CountedState::Exact`], which is not a fallback
/// but the law itself: a retained DECIMAL declines because reading it as a float
/// can round it (jqf's exact `3.0000000000000001` countdown spends four items
/// where a rounded one would spend three), a non-integral float declines because
/// its stopping point is `ceil`/`floor` rather than itself, and a non-number
/// declines because its first payment is a type error.
///
/// A count of `0` under [`CountedKind::Limit`] also declines. The enclosing
/// `if $n > 0` gate makes it unreachable, and declining rather than encoding it
/// keeps the counter's invariant ("a `Limit` counter is at least one") true by
/// construction instead of by argument.
pub(crate) fn classify_count(kind: CountedKind, count: Value) -> CountedState {
    let Value::Number(number) = &count else {
        return CountedState::Exact(count);
    };
    let whole = match number.category() {
        jqf_data::NumberCategory::Integer => number
            .to_integer()
            .and_then(|integer| integer.as_str().parse::<u64>().ok())
            // An integer too wide for `u64` is unreachable by any real source,
            // and it is non-negative here (the sign gates already refused the
            // rest), so it saturates rather than declining.
            .or_else(|| {
                number
                    .to_integer()
                    .filter(|integer| !integer.as_str().starts_with('-'))
                    .map(|_| u64::MAX)
            }),
        jqf_data::NumberCategory::Float => {
            let raw = number.as_float().map_or(f64::NAN, jqf_data::Float::get);
            #[allow(
                clippy::float_cmp,
                reason = "an exact equality against the floor IS the integrality test; a \
                          tolerance would admit counts whose stopping point is not their own \
                          value"
            )]
            if raw.is_finite() && raw >= 0.0 && jqf_builtins::semantics::arith::floor_binary64(raw) == raw {
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "the guard bounds the value to a whole non-negative float; the \
                              saturating comparison covers the rest"
                )]
                let whole = if raw >= 18_446_744_073_709_551_616.0 {
                    u64::MAX
                } else {
                    raw as u64
                };
                Some(whole)
            } else {
                None
            }
        }
        jqf_data::NumberCategory::Decimal => None,
    };
    match whole {
        Some(0) if matches!(kind, CountedKind::Limit) => CountedState::Exact(count),
        Some(whole) => CountedState::Counter(whole),
        None => CountedState::Exact(count),
    }
}

/// Engine-side machine failure surfaced through the run entry.
///
/// None of these are semantic outcomes: the parity mismatch classes live on
/// [`EngineRunError`]'s typed arms. These are the machine's own failures
/// (cooperative control, a rejected ledger charge, or a broken internal
/// contract), which map onto [`CodecError`] at the public poll boundary.
#[derive(Debug)]
pub(crate) enum EngineError {
    /// Cooperative control (cancellation or deadline) ended the request.
    Control(ControlError),
    /// The request ledger rejected charging the frame stack.
    Resource(ResourceError),
    /// A broken internal contract, named for diagnostics. Test-only: the
    /// machine's own navigation contract violations surface directly on
    /// [`EngineRunError::Codec`] from [`stage`], never through this variant,
    /// which exists solely to carry the failure of the
    /// [`StepOutcome::EmitThenFail`] unit-test arm.
    #[cfg(test)]
    Internal(&'static str),
}

impl EngineError {
    /// Maps a machine failure onto the codec-core error channel.
    pub(crate) fn into_codec_error(self) -> CodecError {
        match self {
            Self::Control(error) => CodecError::from(error),
            Self::Resource(error) => CodecError::from(error),
            #[cfg(test)]
            Self::Internal(contract) => CodecError::new(CodecFailureKind::InternalContractViolation { contract }),
        }
    }
}

impl From<EngineError> for EngineRunError {
    fn from(error: EngineError) -> Self {
        Self::Codec(error.into_codec_error())
    }
}

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

/// Which phase a [`GraphFrame::EngineGen`] drive is evaluating.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EngineGenPhase {
    /// `init`: the initial-state filter, run once over the seed input. An empty
    /// init exhausts the cursor; a second output raises the cardinality error.
    Init,
    /// `update`: the state-update filter, run with dot = the current state. An
    /// empty update EXHAUSTS the cursor forever; a second output raises.
    Update,
    /// `extract`: the extract filter, run with dot = the new state. Its single
    /// output is the pull's emitted value; an empty extract emits nothing and
    /// the drive continues to the next update; a second output raises.
    Extract,
}

/// The [`GraphFrame::EngineRng`] drive's phases.
#[derive(Clone, Copy)]
pub(crate) enum EngineRngPhase {
    /// The seed graph is being evaluated; its single exact-integer output
    /// arms the generator (an empty seed exhausts the cursor forever).
    Seed,
    /// The generator is armed: each resumption routes one float.
    Emit,
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

/// The complete outcome of one atomic step transition.
///
/// Shared machine vocabulary. [`StepOutcome::EmitThenFail`] is TEST-ONLY: no
/// compiled program produces an atomic emit+fail (the fan-out machine surfaces
/// emitted-prefix-then-error as a later-poll `Err`, not an emit+fail in one
/// transition), so only its unit test constructs the arm — pinning that the
/// driver publishes the item before it surfaces the failure.
#[derive(Debug)]
pub(crate) enum StepOutcome<'source> {
    /// The transition produced exactly one ordered output.
    Emit(EngineResult<'source>),
    /// The transition emitted one ordered item and then failed atomically. The
    /// item is published before the failure surfaces, preserving the prefix.
    #[cfg(test)]
    EmitThenFail {
        item: EngineResult<'source>,
        error: EngineError,
    },
}

/// One driver advance that produced no failure.
///
/// A failure is the `Err` of [`EngineRunStream::resume`], which — for the
/// stream's emitted-prefix-preserving failures — surfaces only after the prefix
/// items were already returned as [`RunStep::Emit`] on earlier advances.
#[derive(Debug)]
pub(crate) enum RunStep<'source> {
    /// One ordered output item.
    Emit(EngineResult<'source>),
    /// The work slice was exhausted; the caller replenishes and resumes.
    Pending,
    /// The result stream is complete.
    Complete,
}

/// One cooperative observation from the engine result stream.
///
/// The public poll surface the SDK adapter maps onto its own ordered-item poll.
/// A failure is the `Err(EngineRunError)` of [`EngineRunStream::poll`].
#[derive(Debug)]
pub enum RunPoll<'source> {
    /// The stream needs another cooperative entry before yielding a result.
    Pending,
    /// The next ordered semantic result.
    Item(EngineResult<'source>),
    /// Stable end of the ordered result stream.
    Complete,
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

/// One work item for the graph machine's single pending slot.
pub(crate) enum GraphTask<'source> {
    /// Evaluate `node` over `input`; a leaf it produces walks the frame prefix
    /// `[0..cont]` for its consumer.
    Eval {
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
    },
    /// Descend a `Stage` node's steps from `at` over `value`; a leaf walks
    /// `[0..cont]`.
    Descend {
        node: ProgramNodeId,
        value: EngineResult<'source>,
        at: usize,
        cont: usize,
    },
    /// Route an already-produced value through the frame prefix `[0..cont]` — the
    /// finish transition for a completed owned container (a `CollectArray` array or
    /// a `ConstructObject` object), routed through its destination's own
    /// out-continuation.
    Route { value: EngineResult<'source>, cont: usize },
}

/// The RIGHT-OUTER argument law, ONE implementation.
///
/// The argument law: a call answers once per COMBINATION of its arguments'
/// outputs, in the order a right-outer Cartesian product produces — the LAST
/// argument's outputs vary fastest, the FIRST argument's slowest
/// (`setpath((["a"],["b"]); (1,2))` and `def f($a;$b): [$a,$b];
/// f((1,2);(10,20))` pin it). Every drive
/// that answers a call from its arguments' outputs — the graph machine's
/// pure-value laws ([`GraphMachine::emit_argument_product`]), path mode's
/// frozen builtin dispatch (`each_argument_combination`), the callable tail
/// trampoline, and (by construction, through the same iteration) the lazy
/// math frames — goes through this ONE function, so the law cannot diverge
/// between drives.
///
/// The LAST combination selects each argument's last remaining output and
/// TAKES it, so the ordinary single-output call never clones an argument.
///
/// Returns the FIRST apply error as the caller's pending error after every
/// earlier combination was applied — the ordered-prefix law the callers route
/// before raising, exactly as the floor publishes a partial prefix before an
/// error.
pub(crate) fn for_each_argument_combination<F>(
    evaluated: &mut [Vec<Value>],
    mut apply: F,
) -> Result<Option<EngineRunError>, EngineRunError>
where
    F: FnMut(&mut Vec<Value>) -> Result<(), EngineRunError>,
{
    let total = evaluated
        .iter()
        .try_fold(1usize, |count, outputs| count.checked_mul(outputs.len()))
        .ok_or_else(EngineRunError::allocation_failure)?;
    let mut pending = None;
    for index in 0..total {
        let last = index + 1 == total;
        let mut combination = Vec::new();
        combination
            .try_reserve_exact(evaluated.len())
            .map_err(|_| EngineRunError::allocation_failure())?;
        let mut remainder = index;
        // The digits run in REVERSE argument order (the LAST argument is the
        // least significant digit, so its outputs vary fastest — the call
        // law), but the combination itself stays in ARGUMENT order: the
        // selected values are collected reversed and then flipped back, so a
        // law always receives `arguments[0]` as its first argument.
        let mut selected = Vec::with_capacity(evaluated.len());
        for outputs in evaluated.iter_mut().rev() {
            let position = remainder % outputs.len();
            remainder /= outputs.len();
            // The LAST combination selects every argument's LAST output and
            // nothing reads the vectors after it, so it TAKES them: the
            // single-combination call — the shape of nearly every call —
            // never clones an argument at all.
            selected.push(if last {
                outputs.swap_remove(position)
            } else {
                outputs[position].clone()
            });
        }
        for value in selected.into_iter().rev() {
            combination.push(value);
        }
        if let Err(error) = apply(&mut combination) {
            pending = Some(error);
            break;
        }
    }
    Ok(pending)
}

/// One frame of the graph machine's unified continuation stack.
///
/// A produced leaf value walks the frame prefix `[0..cont]` (top-down) for the
/// first CONSUMER frame — [`GraphFrame::FlatMapBody`], [`GraphFrame::Collect`],
/// [`GraphFrame::ObjectKey`], or [`GraphFrame::ObjectValue`] — which consumes it;
/// [`GraphFrame::Each`], [`GraphFrame::Choice`], and [`GraphFrame::ObjectDest`]
/// pass emissions through (they are producers/anchors the walk skips). A frame at
/// index `k` always has its own children route with a `cont` no larger than `k`,
/// so the prefix it walks is stable while it lives.
#[allow(
    clippy::large_enum_variant,
    reason = "the CallableBody arm carries the nested machine by value on purpose: it IS the \
              body evaluation's resumable state, and boxing it would add an unaccounted heap \
              allocation per callable level on the very lane the lazy drive exists for"
)]
/// How a [`GraphFrame::PathPublish`] frame hands its collected paths on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PathFinish {
    /// `path(f)`: the paths stream through the call's out-continuation, and a
    /// body raise publishes the prefix before raising.
    Stream,
    /// The assignment's `_modify` drive: paths go to the fold, a body failure
    /// propagates (the assignment publishes nothing).
    Assignment,
}

/// The `_modify` drive an assignment-mode `PathPublish` completes into.
pub(crate) struct AssignmentTarget {
    /// The update graph (`= RHS` or `|= RHS`).
    pub(crate) update: ProgramNodeId,
    /// The assignment mode (set vs update).
    pub(crate) mode: ModifyMode,
    /// The original input, retained as the fold's initial state.
    pub(crate) root: Value,
    /// The routing position the fold's one document routes through.
    pub(crate) out_cont: usize,
}

pub(crate) enum GraphFrame<'program, 'source> {
    /// A `.[]` fan-out in progress: iterate `container`'s children (located or
    /// owned), descending each through `stage` from `next_at`; children route with
    /// `cont`.
    Each {
        container: Container<'source>,
        cursor: usize,
        stage: ProgramNodeId,
        next_at: usize,
        cont: usize,
    },
    /// The INDEXED half of a `.[]` fan-out: iterate only the children a
    /// recognized correlated scan's sorted keyed index matched, in ascending
    /// ORIGINAL position, routing each with `cont`.
    ///
    /// It is [`GraphFrame::Each`] with the cursor walking an equal-key RUN of
    /// `joins[slot]`'s entries instead of the container itself, and with no
    /// residual: both source rows require the `.[]` to be its stage's LAST step,
    /// so the child IS the value the frame routes and no `Descend` task is
    /// needed. Like `Each` it is a PRODUCER — the emission-routing walk skips it.
    IndexedEach {
        /// The located container the matched positions index into.
        container: Container<'source>,
        /// Which index slot the run lives in; the frame holds positions by
        /// reference so an equal-key run costs no allocation of its own.
        slot: usize,
        /// The next entry to emit, and one past the run's last entry.
        cursor: usize,
        /// One past the run's last entry.
        end: usize,
        /// The output ceiling each emitted child routes through.
        cont: usize,
    },
    /// A completed path-family result set, streamed one value per resumption.
    ///
    /// `path`, `getpath`, `setpath`, and `delpaths` compute their whole output
    /// list eagerly (path mode is a self-contained owned-value evaluation, not a
    /// frame drive), so the frame is a pure PRODUCER: `route` skips it, and each
    /// resumption publishes `values[cursor]` through `out_cont` until the cursor
    /// runs out.
    ///
    /// Eager COMPUTATION is not eager PUBLICATION, and `pending` is what keeps
    /// the two apart. `path/1` is a lazy producer, so a location it already
    /// wrote stays written when a later output of the same argument is not a
    /// location: `path(.a, 1)` prints `["a"]` and only then reports `Invalid
    /// path expression with result 1`. The run therefore hands over the values
    /// it published BEFORE it stopped together with the failure that stopped it,
    /// and the failure is raised HERE — after the last value has been routed,
    /// from the path call's own out-continuation, so it meets exactly the catch
    /// barriers an immediate raise would have met.
    PathEmit {
        values: Vec<Value>,
        cursor: usize,
        pending: Option<EngineRunError>,
        out_cont: usize,
    },
    /// The path-family body's emission consumer (Law 3).
    PathPublish {
        /// The displaced outer register, restored when the frame finishes.
        saved: Option<path_register::PathRegister>,
        /// The collected paths, plus a body raise raised after they stream.
        values: Vec<Value>,
        pending: Option<EngineRunError>,
        /// The routing position every published path routes through.
        out_cont: usize,
        /// `Stream`: the paths go to a `PathEmit`; `Assignment`: the fold.
        finish: PathFinish,
        /// The assignment drive's fold target (only `Assignment` mode).
        assign: Option<AssignmentTarget>,
    },
    /// One tracked navigation's scope marker (the pop restores the register).
    PathStep {
        /// The steps length and addressed value before the navigation.
        steps_len: usize,
        /// The addressed value before the navigation.
        at: Value,
        /// The located node that addressed `at`, restored with it.
        at_node: Option<NodeHandle>,
    },
    /// The `inputs/0` producer: ONE pull from the request's attached input
    /// source per resumption, never a drain.
    ///
    /// `inputs` is `def inputs: try repeat(input) catch …` — a LAZY generator
    /// — and the pull-per-resumption frame is what makes a countdown cut
    /// upstream real: `limit(1; inputs)` drops this frame after one value, so
    /// no further value is pulled and, on a streaming host cursor, no further
    /// input bytes are read. A pull-time parse refusal raises the message text
    /// from the call's own out-continuation, catch-eligible, with every
    /// earlier value's outputs standing — the prefix law: `try [inputs] catch
    /// .` answers the catch.
    InputsEmit {
        /// The output ceiling every pulled value routes through.
        out_cont: usize,
    },
    /// A completed selector result set, streamed one result per resumption.
    ///
    /// `xpath/1` and `css/1` compute their whole result list eagerly (the
    /// selector seam is an owned evaluation over the recovered document), so
    /// the frame is a pure PRODUCER like [`GraphFrame::PathEmit`]: `route`
    /// skips it, and each resumption publishes `results[cursor]` — an element
    /// re-located in the retained authority `input`, or an owned scalar —
    /// through `out_cont` until the cursor runs out. `pending` carries a
    /// failure that stopped the computation (a compile rejection, a budget
    /// exhaustion, a format mismatch) and raises it after the prefix has been
    /// routed — the prefix law, exactly as `PathEmit` keeps `path(.a, 1)`'s
    /// published prefix before its raise.
    SelectorEmit {
        /// The located authority the selected nodes belong to (only element
        /// results re-locate through it; a scalar result is owned).
        input: EngineResult<'source>,
        /// The selected results, in document order: element node ids (the
        /// path law) or owned scalar values (a top-level function call). A
        /// deque because the advance consumes from the FRONT (an owned
        /// scalar cannot be cloned — `Value` is not `Clone`).
        results: alloc::collections::VecDeque<SelectorEmitValue>,
        pending: Option<EngineRunError>,
        out_cont: usize,
    },
    /// The RESUMABLE `paths` / `path(.. | select(f))` producer: the LIFTED
    /// descend walk ([`path_register::PathWalk`]) plus the per-child body
    /// pipeline. `paths/0` publishes each walked child's register path
    /// directly; the streaming `path/1` arm runs the body chain (the
    /// `FlatMap` spine's bodies) as nested machines, one per pull. A cut
    /// between resumptions drops the frame and the walk with it, so
    /// `limit(2; paths(scalars))` walks two children, not the document. The
    /// body machines seed from the binder-slot snapshot exactly as
    /// [`GraphFrame::CallableBody`] does; a body failure routes at the call's
    /// out-continuation with the published prefix standing.
    PathWalkEmit {
        /// The suspended walk.
        walk: path_register::PathWalk<'source>,
        /// The per-child body pipeline (in pipe order), empty for `paths/0`
        /// and `path(..)`.
        pipeline: Vec<ProgramNodeId>,
        /// Pipeline stages resolved once when the walk opens. Per-child
        /// advance loads this instead of re-classifying each Call.
        stages: Vec<WalkStage>,
        /// The live nested machines, one per pipeline stage, bottom-up.
        machines: Vec<(ProgramNodeId, GraphMachine<'program, 'source>)>,
        /// One reusable body machine for the walk. Reseeded per child
        /// instead of allocated and dropped each time.
        reuse: Option<Box<GraphMachine<'program, 'source>>>,
        /// The binder-slot snapshot the body machines are seeded with.
        slots: Vec<Option<EngineResult<'source>>>,
        /// Whether the root path is published (`path(..)` semantics) or
        /// skipped (`paths` — the register-empty exclusion).
        include_root: bool,
        /// The output ceiling every published path routes through.
        out_cont: usize,
    },
    /// The RESUMABLE `tostream` producer: the post-order walk
    /// ([`jqf_builtins::registry::builtins::streams::TostreamWalk`]) emits ONE
    /// pair/marker per resumption, so a wide document's stream costs O(one
    /// item) of residency instead of the whole materialized sequence. A cut
    /// between resumptions drops the frame and the walk with it
    /// (`limit(1; tostream)` walks one emission), exactly as
    /// [`GraphFrame::PathWalkEmit`] does for `paths`.
    TostreamEmit {
        /// The suspended walk.
        walk: jqf_builtins::registry::builtins::streams::TostreamWalk,
        /// The output ceiling every emitted item routes through.
        out_cont: usize,
    },
    /// A keyed collection in progress: the key filter is run ONCE PER CHILD of
    /// `subject`, and every output it produces for that child is captured into
    /// `key` — which is the child's key, because the key is the ARRAY of all
    /// the filter's outputs (`sort_by(.a, .b)` is a two-level sort).
    ///
    /// It is [`GraphFrame::ModifyFold`]'s drive shape over a different cursor:
    /// a CONSUMER while a child's filter is running, and the advancer that
    /// starts the next child (or publishes) when it is exhausted. The five
    /// modes differ only in [`jqf_builtins::semantics::keyed::KEY_MODES`]'s row, never
    /// in this frame.
    ///
    /// `subject` is retained whole because a non-array subject is REJECTED at
    /// the end rather than at the start — `map([f])` iterates an object
    /// perfectly happily and only then fails the both-arrays test, with the
    /// original AND the collected keys in the message.
    KeyedCollect {
        /// Which keyed builtin this collection publishes for.
        mode: jqf_builtins::semantics::keyed::KeyMode,
        /// The materialized input, iterated for the drive and rendered in the
        /// rejection.
        subject: Value,
        /// How many children `subject` has.
        width: usize,
        /// The child whose key is being collected.
        cursor: usize,
        /// The current child's key outputs so far, UNBOXED: the common
        /// single-output case holds the value directly and pays no `Array`
        /// allocation per row (see [`jqf_builtins::semantics::keyed::KeyValue`]).
        key: jqf_builtins::semantics::keyed::KeyValue,
        /// One entry per child already keyed, in INPUT order. When the
        /// request spills, entries stay bounded by the
        /// budget: each flush writes a sorted run and clears the Vec.
        entries: Vec<jqf_builtins::semantics::keyed::KeyedEntry>,
        /// The runs flushed so far; non-empty means the collection is in
        /// spill mode and the merge order replaces the in-memory sort.
        spill_runs: Vec<jqf_resource::RunId>,
        /// Whether the spill has DECLINED for this collection (a non-scalar
        /// key, or no store/budget): sticky, so the flush is never retried.
        spill_declined: bool,
        /// The running encoded-key byte count of `entries`; the spill
        /// trigger meters it with one residency floor per entry.
        spill_key_bytes: usize,
        /// The key filter.
        key_graph: ProgramNodeId,
        /// The output ceiling the published value routes through.
        out_cont: usize,
    },
    /// An argument consumer that ANSWERS: each output the argument filter
    /// produces is answered INDEPENDENTLY against the retained subject,
    /// publishing one value each. `bsearch/1`, `flatten/1` and `has/1` share it
    /// and differ only in [`AnswerSubject`]'s row, never in this frame — which
    /// is what keeps the two exhaustive frame walks at one arm for all three.
    ///
    /// The subject is checked per ANSWER and not at the start, which is the
    /// order: `null | bsearch(empty)` publishes nothing and raises nothing,
    /// while `null | bsearch(1)` raises `null (null) cannot be searched from`.
    ArgumentAnswer {
        /// The retained input every answer is computed against.
        subject: AnswerSubject<'source>,
        /// The output ceiling each answer routes through.
        out_cont: usize,
    },
    /// The OUTER half of `setpath(P; V)`'s streaming drive: the VALUE
    /// argument is the outer loop and the PATH argument the inner one — the
    /// right-outer product: `setpath((["b"],["c"]); (1,2))` answers
    /// `b:1, c:1, b:2, c:2` and `setpath(error; empty)` answers nothing at
    /// all. Each value output opens a [`GraphFrame::PathSetPair`]
    /// inner drive over the path argument; when the value generator is
    /// exhausted the frame pops silently, exactly as [`GraphFrame::MathArgs`]
    /// pops when its outer argument is exhausted.
    PathSetValue {
        /// The materialized document every pair is written against.
        root: Value,
        /// The PATH argument node, driven per value output.
        path_arg: ProgramNodeId,
        /// The original input the PATH argument evaluates over (located when
        /// the request was), so `setpath(path(.name); v)` can still see
        /// markup name facts.
        path_input: EngineResult<'source>,
        /// The output ceiling each answer routes through.
        out_cont: usize,
    },
    /// The INNER phase of `setpath(P; V)`'s streaming drive: one value
    /// output is held, the PATH argument runs into this frame, and every path
    /// output publishes the document rewritten at that `(path, value)` pair.
    /// A CONSUMER that re-emits, exactly like [`GraphFrame::ArgumentAnswer`]:
    /// each path output is answered and published in the same transition, and
    /// when the path generator is exhausted the frame pops silently, letting
    /// the value generator's continuation produce its next output.
    PathSetPair {
        /// The materialized document every pair is written against.
        root: Value,
        /// The outer value this inner drive is answering for.
        value: Value,
        /// The output ceiling each answer routes through.
        out_cont: usize,
    },
    /// One math /2 or /3 call's argument nesting: the OUTER arguments (all but
    /// the first) are consumed rightmost-first, and the FIRST argument is
    /// driven per complete outer combination — the RIGHT-outer Cartesian
    /// order, the same law [`GraphFrame::BinaryRight`]/[`GraphFrame::BinaryLeft`]
    /// implement for operators (`pow((2,3);(4,5))` answers
    /// `16 81 32 243`: for each right output, all left outputs).
    ///
    /// The piped value is NEVER one of the operands: once all parenthesized
    /// arguments are present, the input is ignored entirely (`"x" | hypot(3;4)`
    /// is `5`, and `"x" | hypot(3;4)` does not even type-check the input). The
    /// arguments are ordinary filters over the SAME input, which is retained in
    /// the frame and re-borrowed per nested evaluation.
    MathArgs {
        /// Which /2 or /3 law the call dispatches.
        law: MathCallLaw,
        /// The remaining outer argument nodes, rightmost-first: the last
        /// element is the one currently being evaluated.
        outer: Vec<ProgramNodeId>,
        /// The outer values consumed so far, in consumption order (rightmost
        /// first): for a /2 call the single element is the RIGHT operand, for
        /// `fma` the pair is `[z, y]`.
        collected: Vec<Value>,
        /// The FIRST (innermost) argument node, driven once per complete outer
        /// combination.
        inner: ProgramNodeId,
        /// The retained original input every argument filter runs against.
        input: EngineResult<'source>,
        /// The output ceiling every computed result routes through.
        out_cont: usize,
    },
    /// The innermost phase of a math call: every output of the FIRST argument
    /// is combined with the collected outer values and published. A CONSUMER
    /// that re-emits, exactly like [`GraphFrame::ArgumentAnswer`]: each inner
    /// output is computed and routed in the same transition, and the frame
    /// pops silently when the first argument's generator is exhausted.
    MathCompute {
        /// Which /2 or /3 law the call dispatches.
        law: MathCallLaw,
        /// The outer values, rightmost-first (see [`GraphFrame::MathArgs`]).
        outer: Vec<Value>,
        /// The output ceiling each computed result routes through.
        out_cont: usize,
    },
    /// `walk`'s per-container rebuild: the mapped filter is run over every child
    /// of `subject` BOTTOM-UP, and the rebuilt container is then run through
    /// `filter` itself.
    ///
    /// The definition is `def w: if type == "object" then map_values(w) elif
    /// type == "array" then map(w) else . end | f; w`, and the two arms differ in
    /// exactly the way `map` and `map_values` differ — which is why one frame
    /// carries both through [`WalkRebuild`]. An ARRAY collects EVERY output of
    /// each child's walk (so `[1] | walk(., .+1)` widens to `[1,2]`); an OBJECT
    /// keeps the FIRST output per key and cuts the rest, and drops the key
    /// entirely when the child walk produced none.
    ///
    /// The recursion lives in this stack and never in the Rust one: opening a
    /// walk DESCENDS by pushing frames, and finishing one pops.
    Walk {
        /// The original container, read for its children and its key names.
        subject: Value,
        /// How many children `subject` has.
        width: usize,
        /// The child whose walk is running.
        cursor: usize,
        /// The rebuild so far, in the arm's own shape.
        rebuild: WalkRebuild,
        /// The mapped filter, re-run at every level.
        filter: ProgramNodeId,
        /// The output ceiling the rebuilt-and-filtered container routes through.
        out_cont: usize,
    },
    /// `map_values`'s single-level rebuild: the mapped filter runs over each
    /// member of the TOP container once (no descent), the FIRST output per
    /// member wins and the rest are cut, a member whose filter produced
    /// nothing is DELETED, and the rebuilt container routes through `out_cont`
    /// WITHOUT a final run of the filter (unlike [`GraphFrame::Walk`], whose
    /// rebuilt container is itself passed through `f`).
    ///
    /// The definition is `def map_values(f): .[] |= f;` — the OBJECT arm's
    /// first-output-per-key law applied to both kinds — which is why both
    /// rebuild arms carry the `taken` flag that `walk`'s object arm alone
    /// carries.
    MapValues {
        /// The original container, read for its children and its key names.
        subject: Value,
        /// How many children `subject` has.
        width: usize,
        /// The child whose update is running.
        cursor: usize,
        /// The rebuild so far, in the arm's own shape.
        rebuild: MapValuesRebuild,
        /// The mapped filter, run once per member.
        filter: ProgramNodeId,
        /// The output ceiling the rebuilt container routes through.
        out_cont: usize,
    },
    /// A pending `Choice` right member: after the left member completes (this
    /// frame reaches the top), evaluate `right` over the retained `input`,
    /// routing with `cont`. The frame is popped as it fires — the right member
    /// takes ownership of the retained input, so no second fork clone is needed.
    Choice {
        right: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
    },
    /// A `FlatMap` consumer: for each value the upstream produces, evaluate `body`
    /// over it, routing the body's outputs with `out_cont` (the `FlatMap`'s own
    /// output ceiling, which skips this frame). Reaching this frame at the top of
    /// the stack means the upstream is exhausted — pop it.
    FlatMapBody { body: ProgramNodeId, out_cont: usize },
    /// A `CollectArray` destination consumer: each value the body produces is
    /// materialized at capture and pushed into `array`. Reaching the top of the
    /// stack means the body is exhausted — the LIFO finish: pop, and route the
    /// completed `Value::Array` through `out_cont`.
    Collect { array: Array, out_cont: usize },
    /// The COUNT-ONLY collect frame ([`ProgramNode::CountCollect`]): the body
    /// drives into this frame exactly as into [`GraphFrame::Collect`], but the
    /// consumer increments `count` instead of pushing an owned value into an
    /// array, and the finish routes the exact integer count.
    CountCollect { count: u64, out_cont: usize },
    /// A `ConstructObject` anchor (a producer the routing walk skips): holds the
    /// constructor's ORIGINAL `input` (re-borrowed for every member producer), the
    /// `ctor` node (member lookup), the depth-synchronized `partial` captures, and
    /// the `out_cont` a finished object routes through. Reaching the top of the
    /// stack means every member is exhausted — pop it (the objects were already
    /// routed).
    ObjectDest {
        input: EngineResult<'source>,
        ctor: ProgramNodeId,
        partial: Vec<(jqf_data::ObjectKey, Value)>,
        out_cont: usize,
        /// Proven at most one Cartesian combination. `build_object` then
        /// moves the partial instead of cloning it. Unproven stays false.
        single_combination: bool,
    },
    /// An object member's KEY consumer: each key the member's key producer emits
    /// starts a fresh value iteration for that key. Reaching the top means the key
    /// producer is exhausted — pop it (backtrack to the enclosing member).
    ObjectKey { dest: usize, member: usize },
    /// An object member's VALUE consumer for a fixed `key`: each value the
    /// member's value producer emits is materialized at capture, recorded in the
    /// destination's `partial`, and either starts the next member or (at the last
    /// member) finishes one object combination. Reaching the top means the value
    /// producer for this key is exhausted — pop it (truncating `partial`).
    ///
    /// The `key` is the member's UNVALIDATED key emission: a key is checked to
    /// be a string only when its value produces, so an empty member's key is
    /// never validated and a value that raises reports its own error first.
    /// The check happens in [`Self::consume_object_value`], per value
    /// output.
    ObjectValue {
        dest: usize,
        member: usize,
        key: EngineResult<'source>,
    },
    /// A `Binary` operator's RIGHT-operand consumer (the outer Cartesian loop):
    /// each right output R starts a fresh LEFT-operand iteration over the retained
    /// original `input`, pairing every left output with R under `op`. The right
    /// output is materialized to `R` at capture and carried in a [`BinaryLeft`]
    /// frame across that iteration. Reaching the top of the stack means the right
    /// operand is exhausted — pop it.
    ///
    /// [`BinaryLeft`]: GraphFrame::BinaryLeft
    BinaryRight {
        /// The operator applied at each pair.
        op: BinaryKind,
        /// The left operand graph, re-evaluated per right output.
        left: ProgramNodeId,
        /// The constructor's ORIGINAL input, re-borrowed for each left iteration.
        input: EngineResult<'source>,
        /// The output ceiling a finished pair routes through (skips this frame).
        out_cont: usize,
    },
    /// A `Binary` operator's LEFT-operand consumer for a fixed right operand
    /// `right` (the inner Cartesian loop): each left output L is paired with
    /// `right` under `op`, the result routed through `out_cont`. Reaching the top
    /// of the stack means the left operand is exhausted for this right output —
    /// pop it (backtrack to the enclosing [`BinaryRight`] for the next right
    /// output).
    ///
    /// [`BinaryRight`]: GraphFrame::BinaryRight
    BinaryLeft {
        /// The operator applied at each pair.
        op: BinaryKind,
        /// The materialized right operand, reused across the left iteration.
        right: Value,
        /// The node the right operand was still located at when it was
        /// materialized, `None` when it was already owned. The equality
        /// short-circuit compares it against a still-located LEFT output to
        /// prove both operands come from the same document node.
        right_handle: Option<NodeHandle>,
        /// The output ceiling a finished pair routes through.
        out_cont: usize,
    },
    /// One `Concat` part's consumer in the right-outer product: this part is
    /// the outer loop for the remaining left parts. Each string output is
    /// either joined with `suffix` (leftmost part) or starts a nested Concat
    /// drive of `parts[0..at]`. Same fan-out as a left-associative `+` chain.
    ConcatPart {
        /// The `Concat` node whose `parts` this frame walks.
        node: ProgramNodeId,
        /// Index of the part whose outputs this frame consumes.
        at: usize,
        /// Materialized string pieces for `parts[at+1..]`, already in order.
        suffix: Vec<Value>,
        /// The constructor's original input, re-borrowed for each inner part.
        input: EngineResult<'source>,
        /// The output ceiling a finished combination routes through.
        out_cont: usize,
    },
    /// A `select` predicate consumer: each predicate output is tested for truth
    /// (false/null falsy, everything else truthy); a truthy output re-borrows the
    /// retained original `input` and routes it through `out_cont` as one emission,
    /// a falsy one is dropped. Prior truthy emissions stand if the predicate later
    /// errors (the ordered-stream property). Reaching the top of the stack means
    /// the predicate is exhausted — pop it.
    SelectBody {
        /// The call's ORIGINAL input, re-borrowed per truthy emission.
        input: EngineResult<'source>,
        /// The output ceiling a selected emission routes through (skips this
        /// frame, so a nesting consumer such as a `map` body sees it).
        out_cont: usize,
    },
    /// A `Conditional` condition consumer: each condition output selects an arm.
    /// A truthy (non-`false`/`null`) output re-borrows the retained original
    /// `input` and evaluates `consequent` over it; a falsy output evaluates
    /// `alternative`. Either arm's outputs route through `out_cont` (skipping this
    /// frame). Prior arm emissions stand if the condition later errors. Reaching
    /// the top of the stack means the condition is exhausted — pop it.
    ConditionalBody {
        /// The arm evaluated for a truthy condition output.
        consequent: ProgramNodeId,
        /// The arm evaluated for a falsy condition output.
        alternative: ProgramNodeId,
        /// The conditional's ORIGINAL input, re-borrowed for each selected arm.
        input: EngineResult<'source>,
        /// The output ceiling a selected arm's outputs route through.
        out_cont: usize,
    },
    /// An `Alternative` (`//`) left consumer — a FILTERING PASS-THROUGH routing
    /// class (new): each truthy left output is re-emitted upward through
    /// `out_cont` (and records that a truthy output was seen), each falsy one is
    /// swallowed. Reaching the top of the stack means the left is exhausted: if no
    /// truthy output was seen, `right` is evaluated once over the retained original
    /// `input` (routing through `out_cont`); otherwise the frame is popped and
    /// `right` never runs.
    AlternativeLeft {
        /// The unfiltered fallback, seeded only on left-Complete-with-zero-truthy.
        right: ProgramNodeId,
        /// The alternative's ORIGINAL input, retained for the fallback seeding.
        input: EngineResult<'source>,
        /// Whether any truthy left output has passed through (the had-truthy flag).
        had_truthy: bool,
        /// The output ceiling both truthy left outputs and the fallback route
        /// through (skips this frame).
        out_cont: usize,
    },
    /// A `Logical` (`and`/`or`) LEFT-operand consumer (the outer loop): each left
    /// output either short-circuits (`and` + falsy, or `or` + truthy) — emitting the
    /// boolean of the LEFT's truthiness through `out_cont` without touching the
    /// right — or drives the `right` operand over the retained original `input`,
    /// booleanizing each right output through a [`LogicalRight`] frame. Reaching the
    /// top of the stack means the left operand is exhausted — pop it.
    ///
    /// [`LogicalRight`]: GraphFrame::LogicalRight
    LogicalLeft {
        /// Which short-circuiting operator this node applies.
        operator: LogicalOp,
        /// The right operand graph, evaluated per non-short-circuiting left output.
        right: ProgramNodeId,
        /// The logical's ORIGINAL input, re-borrowed for each right evaluation.
        input: EngineResult<'source>,
        /// The output ceiling every emitted boolean routes through.
        out_cont: usize,
    },
    /// A `Logical` RIGHT-operand consumer (the inner loop): each right output is
    /// booleanized (the RIGHT's truthiness) and routed through `out_cont`. Reaching
    /// the top of the stack means the right operand is exhausted for this left
    /// output — pop it (backtrack to the enclosing [`LogicalLeft`] for the next).
    ///
    /// [`LogicalLeft`]: GraphFrame::LogicalLeft
    LogicalRight {
        /// The output ceiling each emitted boolean routes through.
        out_cont: usize,
    },
    /// A `try` catch BARRIER (a FOURTH routing behavior): body emissions pass
    /// through transparently (the frame sits at or above the body's routing
    /// ceiling and is in the routing skip set), while a catch-eligible RAISE walks
    /// LIFO to the nearest such frame. On a raise the frame is popped BEFORE the
    /// handler seeds (a handler raise bypasses this barrier); a catchless barrier
    /// (`handler` = `None`) swallows the error with no output. Reaching the top of
    /// the stack means the body finished with no uncaught error — pop it.
    Try {
        /// The optional catch handler, seeded with the caught error value at
        /// `out_cont`; `None` swallows.
        handler: Option<ProgramNodeId>,
        /// The output ceiling the handler's outputs route through (the `try`'s own).
        out_cont: usize,
    },
    /// A `label $out` BARRIER — [`GraphFrame::Try`]'s sibling, sharing its
    /// routing behavior exactly: body emissions pass through transparently, and
    /// a catch-eligible raise walks LIFO to it. It differs only in WHICH raises
    /// it answers: a `Try` answers any, a `Label` answers only the break marker
    /// `{"__jq": slot}` carrying ITS slot, and always swallows (a label has no
    /// handler). Reaching the top of the stack means the body finished with no
    /// matching break — pop it.
    ///
    /// Because both kinds live in ONE walk, the ordering falls out for free: in
    /// `label $o | try (break $o) catch .` the `Try` is nearer the top and
    /// catches (rendering `{"__jq":0}`), while in `try (label $o | break $o)
    /// catch .` the `Label` is nearer and swallows.
    /// Unlike `Try` this frame carries NO out-continuation, and the absence is
    /// the design: a `Try` needs one to seed its handler and to run the
    /// `out_cont > raise_cont` enclosure test, whereas a `Label` has no handler
    /// and matches by SLOT. Slot matching already encodes lexical identity, so a
    /// same-slot frame that failed an enclosure test would be unreachable —
    /// reproducing the test would add a dead branch, not a guard.
    Label {
        /// The label occurrence this barrier answers for.
        slot: LabelSlot,
    },
    /// An `error/1` argument consumer: the FIRST value its argument filter emits is
    /// raised as the program error (`error(f)` raises `f`'s first output). Reaching
    /// the top of the stack means the argument produced zero outputs — pop it
    /// (`error(empty)` raises nothing). `out_cont` is the `error/1`
    /// call's routing position, used to scope which `try` catches the raise.
    ErrorArg {
        /// The `error/1` call's out-continuation (the raise's routing position).
        out_cont: usize,
    },
    /// A `Bind` source consumer: each value the binder's source produces is
    /// written into `slot` in its own representation (a located source binds
    /// zero-copy) and `body` is then evaluated over a re-borrow of the binder's
    /// ORIGINAL input (a binding does not change dot). Reaching the top of the
    /// stack means the source is exhausted — pop it.
    BindBody {
        /// The env slot this binder occurrence writes.
        slot: VarSlot,
        /// The body evaluated once per bound value.
        body: ProgramNodeId,
        /// The binder's ORIGINAL input, re-borrowed for each body run.
        input: EngineResult<'source>,
        /// The output ceiling the body's outputs route through (skips this frame).
        out_cont: usize,
        /// The collected source outputs of a register-active multi-output
        /// source, replayed as bodies once it completes (all source outputs
        /// evaluate frozen, every body unfrozen).
        collected: Vec<Value>,
        /// The next collected output whose body runs.
        cursor: usize,
        /// A PATTERN-FRAME extraction binder: its matcher steps are TRACKED
        /// and its body runs inline per output (a collected replay would pop
        /// the matcher's `PathStep` scope before the body runs).
        frame: bool,
    },
    /// An ENGINE binding's body barrier: the cursor this occurrence
    /// owns lives in the machine's cursor store, and this frame is the RAII
    /// release point — when it pops (body completed, unwound past, or cut) the
    /// cursor at `slot` is released. The body's outputs route THROUGH it to
    /// `out_cont` (it is not a consumer); reaching the top means the body's
    /// single evaluation is complete, so the spent-arm pops it and releases.
    EngineBindBody {
        /// The cursor slot this binding occurrence owns.
        slot: EngineSlot,
        /// The output ceiling the body's outputs route through.
        out_cont: usize,
    },
    /// A PULL on an engine cursor: drives the suspended cursor
    /// machine in the machine's cursor store one resumption per advance — the
    /// `CallableBody` drive shape, so a countdown cut between two resumptions
    /// drops this frame and (through the binding barrier) the cursor with it.
    /// A `Next` pull routes ONE value then pops; a `Rest` pull re-pushes
    /// itself after every value until the cursor completes. Reaching the top
    /// with the cursor completed means the pull is spent — pop (a `Next` pull
    /// over an exhausted cursor emits nothing, idempotently).
    EnginePull {
        /// The cursor slot this pull reads.
        slot: EngineSlot,
        /// Whether this pull re-pushes after a value (`.rest`) or pops (`.next`).
        rest: bool,
        /// The output ceiling each pulled value routes through.
        out_cont: usize,
    },
    /// The `~generator(INIT; UPDATE; EXTRACT)` phase drive, owned by
    /// the CURSOR machine seeded at an [`ProgramNode::EngineGenerator`]: a
    /// CONSUMER that captures each phase's outputs with the one-state,
    /// one-emission-per-pull cardinality law. The phase advances when the
    /// current phase's graph is exhausted — init completes into the first
    /// update (or the cursor is exhausted for an empty init), update completes
    /// into extract (or exhausts the cursor for an empty update), and extract
    /// completes back into the next update (an empty-extract cycle emits
    /// nothing and continues; an emitted value was already routed as it
    /// landed).
    EngineGen {
        /// The generator node whose three filters this drive runs.
        node: ProgramNodeId,
        /// The current phase.
        phase: EngineGenPhase,
        /// The current state and its own ledger residency. During `Init`/
        /// `Update` it is `Some` once the phase produced its one output (the
        /// cardinality "already emitted" check reads it); during `Extract` it
        /// holds the NEW state the next update starts from.
        state: Option<StateCell>,
        /// Whether the `Extract` phase already routed its one emission this
        /// cycle (a second extract output raises the cardinality error).
        emitted: bool,
        /// The output ceiling extracted values route through.
        out_cont: usize,
    },
    /// The `~rng($seed)` drive, owned by the CURSOR machine seeded
    /// at an [`ProgramNode::EngineRng`]: the `Seed` phase evaluates the seed
    /// graph once (exactly one exact-integer output; a second raises the
    /// cardinality error, an empty seed exhausts the cursor), then the `Emit`
    /// phase draws ONE float uniform in `[0, 1)` per resumption through the
    /// shared xoshiro256** law — so `~rng(S).next` equals `rand(S)`, and the
    /// second `.next` continues the same stream. The cursor never completes;
    /// a `.rest` pull takes with `limit`.
    EngineRng {
        /// The current phase.
        phase: EngineRngPhase,
        /// The seeded generator, armed by the `Seed` phase's single output.
        rng: Option<Prng>,
        /// The output ceiling drawn values route through.
        out_cont: usize,
    },
    /// A `reduce` INIT consumer: each init output starts ONE complete fold over
    /// the source, sequentially (the catalogued intentional difference: the
    /// reference clobbers dot on a multi-output reduce init). Reaching the top
    /// means the init generator is exhausted — pop it.
    ReduceInit {
        /// The source generator driven once per init output.
        source: ProgramNodeId,
        /// The env slot this loop occurrence writes.
        slot: VarSlot,
        /// The update generator run per source item.
        update: ProgramNodeId,
        /// The loop's ORIGINAL input, re-borrowed for each fold's source drive.
        input: EngineResult<'source>,
        /// The output ceiling the single final state routes through.
        out_cont: usize,
    },
    /// A `reduce` SOURCE consumer: per source item it binds `slot` and drives
    /// `update` with dot = the state TAKEN out of this frame (so zero update
    /// outputs leave the state null — the empty-update null law falls out of the take).
    /// Reaching the top means the source is exhausted: pop and emit the single
    /// final state (an empty source therefore emits the init unchanged).
    ReduceSource {
        /// The env slot this loop occurrence writes.
        slot: VarSlot,
        /// The update generator run per source item.
        update: ProgramNodeId,
        /// The current fold state and its own ledger residency.
        state: StateCell,
        /// The output ceiling the final state routes through.
        out_cont: usize,
    },
    /// A `reduce` UPDATE consumer: each update output OVERWRITES the enclosing
    /// `ReduceSource`'s state, so the LAST output wins. Reaching the top means the
    /// update is exhausted for this item — pop it (the state already stands, and
    /// stays null when the update emitted nothing).
    ReduceUpdate {
        /// Stack index of the enclosing [`GraphFrame::ReduceSource`].
        source_frame: usize,
    },
    /// The keyed-collect drive's SOURCE consumer: the executor-recognized
    /// `reduce s as $row ({}; reduce ($row|key|tostring) as $k (.; .[$k] = $row))`
    /// shape (the INDEX lowering), driven WITHOUT the Modify machinery — the
    /// state is an in-progress [`jqf_data::ObjectBuilder`], never a materialized
    /// Value, and each key lands through the builder's incremental index. The
    /// shape check is conservative: any graph that is not exactly this shape
    /// takes the ordinary `ReduceSource` drive, so every law (multi-key filing,
    /// empty-key no-op, last-wins) is inherited from the same semantics the
    /// generic drive reproduces. Reaching the top means the source is
    /// exhausted: pop, finish the builder into one Object, and route it once.
    KeyedCollectSource {
        /// The env slot this loop occurrence writes.
        slot: VarSlot,
        /// The per-row key generator (`$row|idx|tostring`), run once per source
        /// item with the row bound in `slot`.
        keys: ProgramNodeId,
        /// The in-progress object under construction.
        builder: jqf_data::ObjectBuilder,
        /// The output ceiling the single final object routes through.
        out_cont: usize,
    },
    /// The keyed-collect drive's KEY consumer: each key output is inserted into
    /// the enclosing [`GraphFrame::KeyedCollectSource`]'s builder under the
    /// row bound in its slot (first position, last value). Reaching the top
    /// means the key generator is exhausted for this item — pop it and resume
    /// the source.
    KeyedCollectKey {
        /// Stack index of the enclosing [`GraphFrame::KeyedCollectSource`].
        source_frame: usize,
    },
    /// The STREAMING aggregate drive's SOURCE consumer: the executor-fused
    /// shape `[generator] | group_by(f)` / `min_by(f)` / `max_by(f)` — a
    /// `FlatMap` whose upstream is a `CollectArray` and whose body is a keyed
    /// call — driven WITHOUT the intermediate array. Each generator output is
    /// filed directly into this frame's accumulator (per-key groups, or the one
    /// selection winner), so retention is O(groups + keys) instead of the whole
    /// subject array plus the N-entry table the collect-then-publish path pays.
    /// Reaching the top means the generator is exhausted: pop and publish once,
    /// byte-identical to [`finish_keyed`] over the same elements (the ordering
    /// laws live in [`jqf_builtins::semantics::keyed::KeyedGroups`]).
    KeyedStreamSource {
        /// Which keyed builtin this aggregation publishes for.
        mode: jqf_builtins::semantics::keyed::KeyMode,
        /// The per-element key filter, run once per element.
        key_graph: ProgramNodeId,
        /// The per-key accumulators (`Group` mode).
        groups: jqf_builtins::semantics::keyed::KeyedGroups,
        /// The current winner (`Min`/`Max`): its key and the element itself.
        winner: Option<(jqf_builtins::semantics::keyed::KeyValue, Value)>,
        /// The output ceiling the published value routes through.
        out_cont: usize,
    },
    /// The streaming aggregate drive's KEY consumer: captures one ELEMENT's key
    /// outputs while holding the element itself; filing happens when the filter
    /// exhausts ([`GraphMachine::file_keyed_stream`], [`Self::advance_frame`]'s
    /// own arm). Reaching the top means the key generator is done for this
    /// element — file it and resume the source.
    KeyedStreamKey {
        /// Stack index of the enclosing [`GraphFrame::KeyedStreamSource`].
        source_frame: usize,
        /// The element awaiting its filing.
        pending: Option<Value>,
        /// The element's key outputs so far, under the UNBOXED single-output law
        /// ([`jqf_builtins::semantics::keyed::KeyValue`]).
        key: jqf_builtins::semantics::keyed::KeyValue,
    },
    /// A `foreach` INIT consumer — the `ReduceInit` shape, with the extract.
    ForeachInit {
        /// The source generator driven once per init output.
        source: ProgramNodeId,
        /// The env slot this loop occurrence writes.
        slot: VarSlot,
        /// The update generator run per source item.
        update: ProgramNodeId,
        /// The optional 3-arg extract.
        extract: Option<ProgramNodeId>,
        /// The loop's ORIGINAL input, re-borrowed for each run's source drive.
        input: EngineResult<'source>,
        /// The output ceiling emissions route through.
        out_cont: usize,
    },
    /// A `foreach` SOURCE consumer — the `ReduceSource` shape, except reaching the
    /// top emits NOTHING (a foreach publishes per update output, not at the end).
    ForeachSource {
        /// The env slot this loop occurrence writes.
        slot: VarSlot,
        /// The update generator run per source item.
        update: ProgramNodeId,
        /// The optional 3-arg extract.
        extract: Option<ProgramNodeId>,
        /// The current state and its own ledger residency.
        state: StateCell,
        /// The output ceiling emissions route through.
        out_cont: usize,
    },
    /// A `foreach` UPDATE consumer — a CONSUMER THAT RE-EMITS every consumption
    /// (the fourth consumer flavor under the foundation's emission-routing law;
    /// the precedent is `AlternativeLeft`'s re-route arm, and the routing walk
    /// shape is unchanged). Each update output becomes the next state AND is
    /// published: directly for a 2-arg loop, or through `extract` with dot = the
    /// NEW state for a 3-arg one. Reaching the top pops it, leaving the state
    /// null when the update emitted nothing.
    ForeachUpdate {
        /// Stack index of the enclosing [`GraphFrame::ForeachSource`].
        source_frame: usize,
        /// The optional 3-arg extract each emission detours through.
        extract: Option<ProgramNodeId>,
        /// The output ceiling emissions route through.
        out_cont: usize,
    },
    /// A [`ProgramNode::Counted`] countdown in progress: every item the source
    /// produces reaches this frame, which pays one unit of the count for it and
    /// then publishes it, withholds it, or publishes it and CUTS the source.
    ///
    /// It is a CONSUMER (the routing walk stops here), and the cut is the one
    /// thing it does that no other consumer does: it drops this frame and every
    /// leftover source frame after the published item has been handed to its
    /// consumer. Frames that consumer pushed during the hand-off stay — the same
    /// emit-then-unwind order `first`/`last` get from `label | GEN | (., break)`.
    /// A continuation captured during the hand-off is then reconciled against
    /// the shorter stack; see [`GraphMachine::route`].
    Counted {
        /// The countdown's remaining state.
        state: CountedState,
        /// Which counted law decides this frame's answer per item.
        kind: CountedKind,
        /// The output ceiling a published item routes through (skips this frame,
        /// so a nesting consumer such as a `[…]` collector sees it).
        out_cont: usize,
    },
    /// A [`ProgramNode::Modify`] fold in progress — a PRODUCER the routing walk
    /// skips, like [`GraphFrame::PathEmit`]: it drives one path per resumption
    /// and publishes exactly once, at the end.
    ///
    /// The path set is computed EAGERLY at the node, against the ORIGINAL input,
    /// for the same reason `path/1`'s is (path mode is a self-contained
    /// owned-value evaluation): it is also the compatible shape, because
    /// `_modify` folds over `path(paths)` and therefore cannot see its own
    /// edits in the generator.
    ModifyFold {
        /// The paths to rewrite, in generator order.
        paths: Vec<Value>,
        /// Whether each path may be read by VACATING it (the in-place read) or
        /// must be read intact: the inverse of [`later_extensions`] — a path a
        /// LATER path descends through must not vacate.
        may_vacate: Vec<bool>,
        /// The next path to rewrite.
        cursor: usize,
        /// The evolving document and its own ledger residency.
        state: StateCell,
        /// The paths whose update produced NOTHING, applied through ONE
        /// `delpaths` when the fold completes (what makes `.[] |= empty`
        /// empty the array instead of deleting every other element).
        deferred: Vec<Value>,
        /// The graph producing the new value at each path.
        update: ProgramNodeId,
        /// Whether the update reads the value it replaces.
        mode: ModifyMode,
        /// The output ceiling the single rewritten document routes through.
        out_cont: usize,
    },
    /// A [`GraphFrame::ModifyFold`] step's UPDATE consumer: the FIRST output is
    /// written into the enclosing fold's accumulator at `path`. Reaching the top
    /// of the stack with nothing taken means the update produced no output —
    /// pop it, recording `path` on the fold's deferred deletion list.
    ModifyUpdate {
        /// Stack index of the enclosing [`GraphFrame::ModifyFold`].
        fold_frame: usize,
        /// The path this step is rewriting.
        path: Value,
        /// Whether an output has already landed.
        taken: bool,
        /// Ancestor containers this step vacated, when it did. The matching
        /// write refills them by recorded position. Absent for a declined or
        /// intact read, which still walk through [`semantics_path::set_path`].
        chain: Option<semantics_path::VacatedChain>,
    },
    /// A [`ProgramNode::FactAssign`] step's UPDATE consumer: the FIRST output
    /// becomes the node's new fact payload, recorded as a [`FactDelta`]; the
    /// unchanged document then routes. Reaching the top with nothing taken
    /// means the update produced no output — no delta is recorded (a recorded
    /// narrowing: `|= empty` on a fact payload writes nothing, where a VALUE
    /// update records a deletion).
    FactAssignWrite {
        /// The located node whose fact is written.
        node: NodeId,
        /// The value path naming the node (the SDK's verification re-reads it).
        path: Value,
        /// The fact role selector (`"comment"` or `"attribute"`).
        role: String,
        /// The fact kind (the attribute selector for a `.&name` write; empty
        /// for the comment roles).
        kind: String,
        /// Whether an output has already landed.
        taken: bool,
        /// The unchanged document, routed when the update completes.
        document: EngineResult<'source>,
        /// The output ceiling the document routes through.
        out_cont: usize,
    },
    /// The generator family's ONE frame: a value SOURCE in some state of
    /// [`jqf_builtins::semantics::generate::GENERATORS`], or a consumer of one of the
    /// bound/filter arguments a source needs before it can start.
    ///
    /// Every generator overload reduces to this frame because each is a state
    /// machine that emits one value per resumption — the same contract
    /// [`GraphFrame::Each`] already satisfies for `.[]`. The state decides
    /// whether the routing walk SKIPS the frame (a producer) or stops at it (a
    /// consumer); [`GenerateState::consumes`] is the single place that says
    /// which, so the two walks cannot disagree.
    Generate { state: GenerateState<'source> },
    /// One lazy recursive-definition call in progress: drives the shared body
    /// through a NESTED graph machine, one output per resumption, so a lazy
    /// consumer upstream of the call — a `limit`/`nth` countdown cut, a
    /// `first`-shape break — truncates the recursion at the exact moment it
    /// stops pulling.
    ///
    /// It is a PRODUCER like [`GraphFrame::PathEmit`], and its child is what
    /// makes the cut reach the recursion at all: the child machine owns the
    /// body evaluation's whole resumable state (its own frames, its own env
    /// overlay — the per-level isolation the binder law requires of a shared
    /// body), so dropping this frame — the countdown cut pops everything above
    /// its own index — drops the child and every level below it with it.
    CallableBody {
        /// The shared arena body node every combination re-evaluates.
        body: ProgramNodeId,
        /// The owned dot the body evaluates over (the call's materialized
        /// input), re-cloned once per combination.
        root: Value,
        /// The binder overlay WITHOUT the current combination's parameters: a
        /// snapshot of the caller's env, cloned and extended per combo.
        overlay: Vec<Option<Value>>,
        /// The value-parameter slots the combination values bind, in the
        /// order `combos[combo]` supplies them.
        param_slots: &'program [VarSlot],
        /// The argument combinations not yet fully driven.
        combos: Vec<Vec<Value>>,
        /// The combination currently being driven.
        combo: usize,
        /// The callable depth of the child machine: one more than its driver's,
        /// checked by the child's own `CallDef` arms against
        /// [`CALLABLE_DEPTH_LIMIT`] so the ceiling is shared across the whole
        /// call chain.
        depth: usize,
        /// The nested machine driving the current combination's body, when one
        /// is mid-flight. `None` between combinations.
        ///
        /// Boxed (and charged) because the machine is the frame enum's largest
        /// member by far: held inline it put every `GraphFrame` slot at 744
        /// bytes, and every push on every lane paid that copy — ~10% of the
        /// histogram lane's wall was frame memmove traffic.
        child: Option<Box<GraphMachine<'program, 'source>>>,
        /// The output ceiling a body output routes through.
        out_cont: usize,
        /// The complete filter-parameter env to install on the child machine
        /// (caller env plus this call's captured closures). Empty when the
        /// callable has no filter parameters and the caller had none.
        filter_env: Vec<Option<FilterClosure>>,
    },
    /// A recursive-definition FILTER-parameter use in progress: the machine's
    /// env and filter-env have been replaced with the captured closure's, and
    /// this frame restores them when the captured graph is exhausted (or when
    /// a raise/cut pops it). Transparent to emission routing — captured-graph
    /// outputs are the use's outputs.
    CallFilter {
        /// The env in force before the captured overlay was installed.
        saved_env: Vec<EngineResult<'source>>,
        /// The filter-env in force before the captured closures were installed.
        saved_filters: Vec<Option<FilterClosure>>,
    },
}

/// One captured filter-parameter argument: the argument graph plus the env
/// it was written against. Cloning is O(slots) Rc bumps, not a copy of the
/// overlay chain — each recursive level shares the outer closure by Rc.
///
/// [`Drop`] is iterative: a trampolined `def f(n): f(n)` captures a chain
/// one closure deep per round, and a recursive destructor would overflow
/// the native stack at the tail-round ceiling.
#[derive(Clone, Debug)]
pub(crate) struct FilterClosure {
    /// The call-site argument graph this closure evaluates.
    pub node: ProgramNodeId,
    /// Value-slot snapshot at the call that created this closure.
    pub overlay: alloc::rc::Rc<Vec<Option<Value>>>,
    /// Filter-parameter env at that same call (the outer closures).
    pub filters: alloc::rc::Rc<Vec<Option<FilterClosure>>>,
}

impl Drop for FilterClosure {
    fn drop(&mut self) {
        let mut rest: Vec<alloc::rc::Rc<Vec<Option<FilterClosure>>>> = Vec::new();
        rest.push(core::mem::replace(&mut self.filters, alloc::rc::Rc::new(Vec::new())));
        while let Some(rc) = rest.pop() {
            let Ok(mut env) = alloc::rc::Rc::try_unwrap(rc) else {
                continue;
            };
            for slot in &mut env {
                let Some(inner) = slot.take() else {
                    continue;
                };
                let inner = core::mem::ManuallyDrop::new(inner);
                // SAFETY: `inner` is wrapped in `ManuallyDrop`, so its
                // destructor will not run. Both fields are `Rc` and are
                // read once; overlay is dropped here, filters join `rest`.
                rest.push(unsafe { core::ptr::read(&raw const inner.filters) });
                drop(unsafe { core::ptr::read(&raw const inner.overlay) });
            }
        }
    }
}

/// The retained subject one [`GraphFrame::ArgumentAnswer`] answers against, and
/// which question it answers.
///
/// The row is also the MATERIALIZATION policy. `bsearch` and `flatten` both
/// rebuild from the whole input and materialize once at open time rather than
/// once per answer; `has` reads only the container's shape, so it keeps the
/// input in whatever representation it arrived in and answers a located object
/// through the document's own key index.
pub(crate) enum AnswerSubject<'source> {
    /// `bsearch/1`: the materialized haystack, searched once per target.
    BinarySearch(Value),
    /// `flatten/1`: the materialized subject, re-spliced once per depth.
    Flatten(Value),
    /// `join/1`: the materialized container, re-joined once per separator. The
    /// separator is bound OUTSIDE the fold, so a multi-output separator answers
    /// once per separator — which is exactly what this frame does.
    Join(Value),
    /// `has/1`: the subject as it arrived, read once per key.
    HasKey(EngineResult<'source>),
    /// `getpath/1`: the materialized subject, read once per path the argument
    /// names. The argument is a filter, so the call answers once per argument
    /// output — the law, which is what keeps the answer for an earlier path
    /// published when a later argument output raises.
    GetPath(Value),
    /// `delpaths/1`: the materialized subject, re-deleted once per path SET the
    /// argument names. Same per-argument-output law as `GetPath`; each answer
    /// rebuilds the document from a fresh clone of the subject, so the subject
    /// is only ever read here.
    DelPaths(Value),
    /// `format/1`: the materialized subject, re-rendered once per format name.
    /// Eight of the ten formats stringify it and two read every cell, so there
    /// is nothing shallower to retain.
    Format(Value),
    /// The five argument-taking text laws: the materialized subject, re-read
    /// once per affix. Grouped because the five differ only in the law, and the
    /// law is a `Copy` discriminant the frame carries beside the subject.
    Text {
        /// Which of the five laws this frame answers with.
        law: TextLaw,
        /// The input string every answer is computed against.
        subject: Value,
    },
    /// The three argument-taking date laws: the retained subject (a timestamp
    /// number or parsed-datetime array for `strftime`/`strflocaltime`, a date
    /// string for `strptime`), re-read once per format output.
    Time {
        /// Which of the three laws this frame answers with.
        law: TimeFormatLaw,
        /// The subject every answer is computed against.
        subject: Value,
    },
}

impl AnswerSubject<'_> {
    /// A second handle on the subject, for the answer that is about to be
    /// computed while the frame keeps its own.
    pub(crate) fn try_clone(&self) -> Result<Self, EngineRunError> {
        Ok(match self {
            Self::BinarySearch(value) => Self::BinarySearch(value.clone()),
            Self::Flatten(value) => Self::Flatten(value.clone()),
            Self::Join(value) => Self::Join(value.clone()),
            Self::HasKey(result) => Self::HasKey(result.try_clone().map_err(|_| EngineRunError::allocation_failure())?),
            Self::Format(value) => Self::Format(value.clone()),
            Self::GetPath(value) => Self::GetPath(value.clone()),
            Self::DelPaths(value) => Self::DelPaths(value.clone()),
            Self::Text { law, subject } => Self::Text {
                law: *law,
                subject: subject.clone(),
            },
            Self::Time { law, subject } => Self::Time {
                law: *law,
                subject: subject.clone(),
            },
        })
    }

    /// Answers one argument output against this subject.
    pub(crate) fn answer(&self, target: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
        match self {
            Self::BinarySearch(subject) => order_builtins::bsearch(subject, target, resources),
            Self::Flatten(subject) => reshape_builtins::flatten(subject, Some(target), resources),
            Self::Join(subject) => text_builtins::join(subject, target, resources),
            Self::HasKey(subject) => reshape_builtins::has(subject, target, resources),
            Self::Format(subject) => format_builtins::format(subject, target, resources),
            Self::Text { law, subject } => search_builtins::apply(*law, subject, target, resources),
            Self::GetPath(subject) => semantics_path::get_path(subject, target, resources),
            Self::DelPaths(subject) => jqf_builtins::semantics::pathset::delete_paths(subject, target, resources),
            Self::Time { law, subject } => time_builtins::apply_format(*law, subject, target, resources),
        }
    }
}

/// Which question one [`GraphFrame::ArgumentAnswer`] drive is opening, chosen at
/// eval time and fixed for the life of the frame.
#[derive(Clone, Copy)]
pub(crate) enum AnswerForm {
    /// `bsearch/1`.
    BinarySearch,
    /// `flatten/1`.
    Flatten,
    /// `join/1`.
    Join,
    /// `has/1`.
    HasKey,
    /// `format/1`.
    Format,
    /// The three argument-taking date laws (`strftime`, `strflocaltime`,
    /// `strptime`).
    Time(TimeFormatLaw),
    /// One of the five argument-taking text laws.
    Text(TextLaw),
}

/// Which /2 or /3 math law a [`GraphFrame::MathArgs`] drive is opening, chosen
/// at call dispatch.
#[derive(Clone, Copy, Debug)]
pub(crate) enum MathCallLaw {
    /// One of the fifteen /2 math laws (`hypot`, `pow`, …).
    Bin(math_builtins::MathLawBin),
    /// The one /3 math law (`fma`).
    Tern(math_builtins::MathLawTern),
}

/// What a [`GraphFrame::Counted`] countdown has left to pay, in one of two
/// representations.
///
/// The two are not two laws — they are one law and its specialization. The law
/// is the `foreach f as $item ($n; . - 1; …)` countdown: a state that starts at
/// the bound count and pays `. - 1` per item, compared against `0` under the
/// arithmetic law. [`CountedState::Exact`] IS that law, transcribed onto the
/// same [`jqf_builtins::semantics::binary::apply`] the `Binary` nodes call, so every
/// number model, every rounding rule, and every type error it can raise are the
/// operator's, not this frame's.
///
/// [`CountedState::Counter`] is the specialization that makes the node worth
/// having, and it fires only where the transcription is PROVABLY a plain
/// integer countdown: a count that is an exact integer, or a float that is
/// integral, has `n - k` equal to the integer difference at every step, so the
/// item at which the comparison flips is `n` itself and no per-item arithmetic
/// is needed to find it. A retained DECIMAL never takes it — reading one as a
/// float can round (`3.0000000000000001` reads as `3.0`, and jqf's exact
/// countdown spends four items where the rounded one would spend three) — and
/// neither does a non-integral float, a non-number, or a NaN.
pub(crate) enum CountedState {
    /// How many items are still owed to the count: the exact-integer
    /// specialization. Saturated at [`u64::MAX`], which is the same
    /// observable as an unreachable count — no source emits 2^64 items.
    Counter(u64),
    /// The general countdown's current state, paid down with `. - 1` per item.
    /// It also carries the counts that are not numbers at all: the count passed
    /// its sign gate (the total order puts strings, arrays, objects and `true`
    /// above `0`), so the FIRST item raises the ordinary subtraction type error
    /// and an empty source raises nothing.
    Exact(Value),
}

/// The half-built container one [`GraphFrame::Walk`] is rebuilding, in the arm's
/// own shape.
///
/// The two arms are `map` and `map_values`, and their difference is the whole
/// reason `walk` needs a discriminated rebuild rather than a uniform one.
pub(crate) enum WalkRebuild {
    /// The ARRAY arm (`map(w)`): EVERY output of every child's walk, in order.
    Array(Array),
    /// The OBJECT arm (`map_values(w)`): the FIRST output per key, with a key
    /// whose child walk produced none simply absent.
    Object {
        /// The entries settled so far, in the subject's own key order.
        entries: ObjectBuilder,
        /// Whether the CURRENT key has already taken its one output.
        taken: bool,
    },
}

/// The half-built container one [`GraphFrame::MapValues`] is rebuilding, in
/// the arm's own shape.
///
/// Both arms take the FIRST output per member and delete a member whose
/// filter produced none — the `.[] |= f` law — which is the OBJECT arm's shape
/// applied to both kinds (the ARRAY arm of `walk` differs precisely because
/// `map` collects every output).
pub(crate) enum MapValuesRebuild {
    /// The ARRAY arm: the first output per element, an empty update deleting
    /// the element.
    Array {
        /// The items settled so far, in the subject's own order.
        items: Array,
        /// Whether the CURRENT element has already taken its one output.
        taken: bool,
    },
    /// The OBJECT arm: the first output per key, an empty update deleting the
    /// key.
    Object {
        /// The entries settled so far, in the subject's own key order.
        entries: ObjectBuilder,
        /// Whether the CURRENT key has already taken its one output.
        taken: bool,
    },
}

/// Which of the two loop generators a loop frame is driving.
///
/// They differ in exactly one place — what a TRUTHY condition output means —
/// so they share their three frame states rather than duplicating them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LoopForm {
    /// `while(cond; update)`: truthy emits and continues.
    While,
    /// `until(cond; update)`: truthy emits and STOPS; falsy continues.
    Until,
}

impl LoopForm {
    /// The generator row this form is, so that the loop states can name their
    /// row without a second spelling of the While/Until split.
    const fn generator(self) -> Generator {
        match self {
            Self::While => Generator::While,
            Self::Until => Generator::Until,
        }
    }
}

/// The parts of a `recurse` walk that are the same at every node.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RecurseStep {
    /// The child producer. `None` is `recurse/0`'s `.[]?` — read straight off
    /// the value's members, so a scalar simply has none and the iterate
    /// mismatch the `?` would suppress never arises. `Some(f)` is the authored
    /// filter, whose errors PROPAGATE (`{"a":1} | [recurse(.a)]` raises).
    pub(crate) filter: Option<ProgramNodeId>,
    /// `recurse/2`'s child gate. It gates CHILDREN and never the seed, which is
    /// why `20 | [recurse(.*2; . < 10)]` is `[20]` rather than empty.
    pub(crate) cond: Option<ProgramNodeId>,
    /// The output ceiling every emitted node routes through.
    pub(crate) out_cont: usize,
}

/// One [`GraphFrame::Generate`] frame's state.
pub(crate) enum GenerateState<'source> {
    /// A `range` BOUND consumer — the LEFT-OUTER Cartesian nesting, which is
    /// the inverse of [`ProgramNode::Binary`]'s right-outer product: `from` is
    /// the outer loop, `upto` the middle one and `by` the innermost, so
    /// `range(0,1; 3,4)` runs (0,3), (0,4), (1,3), (1,4).
    ///
    /// Each output of the bound at `depth` fixes that bound and either starts
    /// the NEXT bound's generator or, at the last one, opens the emitter.
    RangeBound {
        /// The `range` call, re-read for its argument list and arity.
        node: ProgramNodeId,
        /// Which authored argument this frame is consuming.
        depth: usize,
        /// `(from, upto, by)` as AUTHORED. They stay VALUES until the last one
        /// lands, because which of `range`'s two laws applies is a fact about
        /// the whole triple.
        bounds: RangeBounds,
        /// The call's ORIGINAL input, re-borrowed for each inner bound.
        input: EngineResult<'source>,
        /// The output ceiling every emitted number routes through.
        out_cont: usize,
    },
    /// The `range` EMITTER: three f64, stepped once per resumption.
    Range {
        cursor: RangeCursor,
        /// The authored `from` number, emitted for the FIRST value so the
        /// literal's spelling survives (see [`generate::RangeLaw::Numeric`]);
        /// `None` after it is spent or when the arity defaulted it.
        first: Option<Number>,
        out_cont: usize,
    },
    /// The `range/3` VALUE emitter — the [`generate::GENERATORS`] Range row's
    /// SECOND law, stepped once per resumption through the total order and
    /// binary `+`. It is a separate state rather than a flag on
    /// [`Self::Range`] because the two laws differ in their TERMINATION class,
    /// which is what [`GenerateState::cooperative`] reads.
    ValueRange { cursor: ValueCursor, out_cont: usize },
    /// A `while`/`until` CONDITION consumer: each condition output decides,
    /// for the retained `current`, whether to emit it, to step it, or neither.
    LoopCond {
        form: LoopForm,
        current: Value,
        cond: ProgramNodeId,
        update: ProgramNodeId,
        out_cont: usize,
    },
    /// `while`'s DEFERRED STEP (a producer): the value was emitted, and the
    /// step that follows it waits here until the emission's own consumers are
    /// finished with it. Reaching the top of the stack runs `update`.
    LoopStep {
        current: Value,
        cond: ProgramNodeId,
        update: ProgramNodeId,
        out_cont: usize,
        /// Where this round's [`Self::LoopCond`] sits, carried across the
        /// deferral so the update consumer inherits it. See
        /// [`Self::LoopUpdate`]'s own field for what reads it.
        cond_index: usize,
    },
    /// A `while`/`until` UPDATE consumer: each update output is one next value,
    /// and starts its own condition test.
    LoopUpdate {
        form: LoopForm,
        cond: ProgramNodeId,
        update: ProgramNodeId,
        out_cont: usize,
        /// Where this round's [`Self::LoopCond`] sits.
        ///
        /// A round is two consumer frames — the condition, then the update — and
        /// [`GraphMachine::tail_step_loop`] has to pop BOTH before the next
        /// round pushes its own. Recording the index when the update frame is
        /// created makes "the frame this round's condition ran in" an exact fact
        /// rather than a guess from adjacency; the index stays valid because a
        /// trim only ever pops frames ABOVE a floor, so nothing below a live
        /// frame moves.
        cond_index: usize,
    },
    /// `repeat`'s round producer: it holds the ORIGINAL input and NEVER
    /// advances it, which is the whole law — `0 | [limit(5; repeat(.+1))]` is
    /// `[1,1,1,1,1]`. Reaching the top of the stack runs `body` again, so the
    /// frame never pops on its own.
    Repeat {
        input: Value,
        body: ProgramNodeId,
        out_cont: usize,
    },
    /// A `recurse` node whose value has been emitted and whose children have
    /// not been walked yet (a producer). Reaching the top of the stack opens
    /// the child walk. The nesting guard rides here so that the recursion is
    /// bounded by the ledger rather than by the Rust stack — which it never
    /// touches, because the walk lives entirely in this frame stack.
    RecurseSeed {
        seed: Value,
        step: RecurseStep,
        depth: OwnedDepthGuard,
    },
    /// `recurse/0`'s child walk (a producer): the members of one already-
    /// emitted value, one recursion per resumption.
    RecurseMembers {
        subject: Value,
        cursor: usize,
        step: RecurseStep,
        /// Held for its `Drop`, not read: the walk's depth is released when
        /// this frame is, which is what ties the guard to the recursion.
        _depth: OwnedDepthGuard,
    },
    /// A `recurse/1`/`recurse/2` child-filter consumer: each output is one
    /// child to recurse into, gated by `step.cond` when there is one.
    RecurseChild {
        step: RecurseStep,
        /// Held for its `Drop`, exactly as [`Self::RecurseMembers`] holds it.
        _depth: OwnedDepthGuard,
    },
    /// A `recurse/2` child-GATE consumer: each truthy condition output admits
    /// the retained `child` into the walk, exactly as `select` would.
    RecurseCond { child: Value, step: RecurseStep },
    /// `combinations/1`'s count consumer, which ACCUMULATES rather than fans
    /// out: `[range(n)] | map($dot)` collects the WHOLE `range(n)` stream
    /// into one dimension vector, so `combinations(1,2)` is three dimensions
    /// (`[range(1,2)]` is `[0,0,1]`) and eight tuples, not two separate
    /// odometers. `width` is that running total; the odometer opens when the
    /// count filter is exhausted, which is also what gives `combinations(empty)`
    /// its one empty tuple.
    CombinationsWidth {
        input: Value,
        width: usize,
        out_cont: usize,
    },
    /// The `combinations` odometer (a producer): one tuple per resumption.
    Combinations {
        subject: Value,
        odometer: Odometer,
        out_cont: usize,
    },
}

impl GenerateState<'_> {
    /// Which row of [`generate::GENERATORS`] this state belongs to.
    ///
    /// The states outnumber the generators — one generator's work is several
    /// steps — so the mapping is many-to-one, and it is the only place the
    /// executor's state machine is tied back to the semantic table.
    const fn generator(&self) -> Generator {
        match self {
            Self::RangeBound { .. } | Self::Range { .. } | Self::ValueRange { .. } => Generator::Range,
            Self::LoopCond { form, .. } | Self::LoopUpdate { form, .. } => form.generator(),
            Self::LoopStep { .. } => Generator::While,
            Self::Repeat { .. } => Generator::Repeat,
            Self::RecurseSeed { .. }
            | Self::RecurseMembers { .. }
            | Self::RecurseChild { .. }
            | Self::RecurseCond { .. } => Generator::Recurse,
            Self::CombinationsWidth { .. } | Self::Combinations { .. } => Generator::Combinations,
        }
    }

    /// Whether resuming this state must first give the ledger a turn.
    ///
    /// Read straight off the table: the cooperative laws are the ones with no
    /// structural bound of their own, so a resumption of one of them is the only
    /// moment a cancellation or a deadline can be observed. A state that runs
    /// its row's SECOND law reads that law's column instead of the primary one —
    /// `range/3` over non-numeric bounds is exactly as unbounded as `while` is.
    pub(crate) fn cooperative(&self) -> bool {
        let row = generate::row(self.generator());
        let termination = match (self, row.second_law) {
            (Self::ValueRange { .. }, Some(second)) => second.termination,
            _ => row.termination,
        };
        matches!(termination, Termination::Cooperative)
    }

    /// Whether the emission-routing walk STOPS at this state.
    ///
    /// A consumer state is waiting on one of its own arguments; a producer
    /// state is mid-stream and must be walked past exactly as `Each` is, so
    /// that a value emitted from deeper in the program reaches the consumer
    /// that asked for it rather than being swallowed here.
    pub(crate) const fn consumes(&self) -> bool {
        match self {
            Self::RangeBound { .. }
            | Self::LoopCond { .. }
            | Self::LoopUpdate { .. }
            | Self::RecurseChild { .. }
            | Self::RecurseCond { .. }
            | Self::CombinationsWidth { .. } => true,
            Self::Range { .. }
            | Self::ValueRange { .. }
            | Self::LoopStep { .. }
            | Self::Repeat { .. }
            | Self::RecurseSeed { .. }
            | Self::RecurseMembers { .. }
            | Self::Combinations { .. } => false,
        }
    }

    /// The generator's own out-continuation: the ceiling every value the
    /// generator emits routes through. Every state carries it — a bound
    /// consumer and an emitter alike — so a raise escaping a bound argument's
    /// evaluation can be floored to the generator call's position by the
    /// sibling-boundary clamp ([`GraphMachine::raise_boundary`]).
    pub(crate) const fn out_cont(&self) -> usize {
        match self {
            Self::RangeBound { out_cont, .. }
            | Self::Range { out_cont, .. }
            | Self::ValueRange { out_cont, .. }
            | Self::LoopCond { out_cont, .. }
            | Self::LoopStep { out_cont, .. }
            | Self::LoopUpdate { out_cont, .. }
            | Self::Repeat { out_cont, .. }
            | Self::CombinationsWidth { out_cont, .. }
            | Self::Combinations { out_cont, .. } => *out_cont,
            Self::RecurseSeed { step, .. }
            | Self::RecurseMembers { step, .. }
            | Self::RecurseChild { step, .. }
            | Self::RecurseCond { step, .. } => step.out_cont,
        }
    }
}

/// One owned value held across machine steps.
///
/// This is the loop state register: a fold overwrites its state once per
/// iteration, so the register holds only the current state value.
pub(crate) struct StateCell {
    pub(crate) value: Value,
}

/// One recognized correlated scan's index slot.
///
/// The cache is bounded BY CONSTRUCTION — one slot per row of the compile-time
/// [`CorrelatedScan`] table, allocated once when the machine first needs it —
/// so it needs no eviction policy and cannot grow with the document.
///
/// `container` is the memo's `(DocumentId, NodeHandle, key-path fingerprint)`
/// cache key, complete: a [`jqf_data::NodeHandle`] CARRIES its `DocumentId`
/// (a process-unique `DocumentId` from a counter that fails rather than wraps,
/// plus the revision), so handle equality alone proves "the same node of the
/// same document", and the key PATH is a static property of the scan node that
/// selects the slot, so it cannot differ between two uses of one slot.
#[derive(Default)]
pub(crate) struct JoinSlot {
    pub(crate) container: Option<NodeHandle>,
    pub(crate) index: Option<JoinIndex>,
    /// The user-declared index slot this scan serves, when
    /// a declared `declare_index/2` covers this slot's (container, key path). The
    /// store owns the [`JoinIndex`]; the slot names it. `None` for a slot
    /// that owns its own warm-up build.
    pub(crate) user: Option<usize>,
    /// Set when a build DECLINED. The reasons a build declines — a key-path
    /// navigation that raises, a refused ledger grant — are properties of the
    /// container and the key path, so retrying once per outer element would pay
    /// a whole extra scan for the same answer.
    pub(crate) poisoned: bool,
}

impl JoinSlot {
    /// The slot's state on FIRST sight of a container: remembered, but not yet
    /// indexed.
    pub(crate) const fn cold(container: NodeHandle) -> Self {
        Self {
            container: Some(container),
            index: None,
            user: None,
            poisoned: false,
        }
    }
}

/// The sorted keyed MULTIMAP over one container's children.
///
/// Sorted rather than hashed, and that is the load-bearing choice: over NaN-FREE
/// keys jqf's `==` IS [`jqf_builtins::semantics::order::total_cmp`]` == Equal`
/// ([`jqf_builtins::semantics::order::semantic_eq`]), so an index ordered by
/// `total_cmp` is equivalence-safe BY CONSTRUCTION — its notion of key equality
/// cannot drift from the operator's because they are one function. A hash index
/// would need a `Hash` provably agreeing with `total_cmp` (int/float
/// unification, key-order-insensitive objects) to buy away the ~15 comparisons a
/// binary search over 20,000 entries costs.
///
/// The NaN-free qualifier is ENFORCED rather than assumed: `==` reads the
/// observable law, under which no NaN-bearing
/// pair is ever equal, while the order an index can be built on must stay the
/// consistent one. A NaN key would therefore be found by the index and rejected
/// by the operator, so the build DECLINES the whole index on the first one and
/// the naive `==` scan answers instead — the same decline every other
/// unrepresentable shape here takes. Checking the KEYS alone suffices: a probe's
/// NaN can never compare `Equal` to a NaN-free key under EITHER law, since the
/// two only disagree where both sides carry one.
///
/// A MULTIMAP, not a map: the naive scan visits EVERY duplicate, so every
/// duplicate is an entry. (`INDEX/2` keeps the last duplicate and stringifies
/// its keys — a different rewrite for a different program.)
pub(crate) struct JoinIndex {
    /// Entries ordered by `total_cmp` with a STABLE sort, so each equal-key run
    /// stays in ascending original position and walking it reproduces document
    /// order exactly. That is the whole ordering argument.
    pub(crate) entries: Vec<JoinEntry>,
}

/// One child's key paired with its position in the container.
pub(crate) struct JoinEntry {
    pub(crate) key: Value,
    pub(crate) position: u32,
}

/// How many times a recognized correlated scan took the INDEXED route and a
/// recognized anti-join answered from the index — a TEST-ONLY engagement
/// probe. The routes' output is byte-identical to the naive scans', so only a
/// counter can distinguish them; the engagement tests assert it moved.
pub(crate) static JOIN_ENGAGEMENTS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// How many times a recognized partial-sort row took the bounded-heap path
/// instead of the full-sort floor — a TEST-ONLY engagement probe, sibling of
/// [`JOIN_ENGAGEMENTS`]. The route's output is byte-identical to the floor's,
/// so only a counter can tell them apart; the engagement tests read it through
/// before/after deltas. Never a `--diagnostics` surface.
pub(crate) static PARTIAL_SORT_ENGAGEMENTS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// How many times a `[generator] | group_by/min_by/max_by` pipe fused into the
/// streaming aggregate drive instead of materializing the collect — a
/// TEST-ONLY engagement probe, sibling of [`PARTIAL_SORT_ENGAGEMENTS`]. The
/// drive's output is byte-identical to the collect-then-key path's, so only a
/// counter (or the ledger peak) can tell them apart. Never a `--diagnostics`
/// surface.
pub(crate) static STREAMED_KEYED_ENGAGEMENTS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// How many times a recognized scan was ROUTED THROUGH a USER-declared index
/// rather than the slot's own warm-up build — a
/// TEST-ONLY engagement probe, separate from [`JOIN_ENGAGEMENTS`] so a
/// user-index test can assert ITS route engaged without depending on the
/// auto-index counters.
pub(crate) static USER_INDEX_PROBES: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// One step of a DECLARED index's key path: the recognizer's row grammar
/// (object keys and array indices, never `?`), converted from a [`StageStep`]
/// on BOTH sides of the match so the declaration and the scan compare the same
/// canonical form.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum UserKeyStep {
    Key(String),
    Index(i64),
}

/// One USER-declared index (`declare_index/2`): a sorted keyed multimap over a LOCATED
/// container, keyed on the container's node identity plus the static key path.
/// Built ONCE at the declaration's evaluation; every later recognized scan over
/// the same (container node, key path) probes it instead of building its own.
/// The key path is part of the identity, so two declarations on one container
/// keyed on different fields coexist.
pub(crate) struct UserIndexSlot {
    pub(crate) container: NodeHandle,
    pub(crate) key_steps: Vec<UserKeyStep>,
    pub(crate) index: JoinIndex,
}

/// Whether one recognized scan took the indexed route.
pub(crate) enum ScanRoute {
    /// The route was taken: the frame is in place (or, for an empty match run,
    /// deliberately absent), and the naive source must NOT be seeded.
    Indexed,
    /// Declined: drive the source exactly as an unrecognized scan would.
    Naive,
}

impl StateCell {
    /// Takes the held value out, releasing its residency and leaving `null`
    /// behind — exactly the reduce/foreach law that an update emitting NOTHING
    /// nulls the state, and the reason no separate emitted flag exists.
    pub(crate) fn take(&mut self) -> Value {
        core::mem::replace(&mut self.value, Value::Null)
    }
}

/// The graph executor driving a `Choice`/`FlatMap` residual to an ordered stream.
///
/// `nodes` is the borrowed program arena; the current task lives in `pending`
/// (a single slot); `frames` is the unified continuation stack.
/// `entry_offset` is the pushdown prefix length the codec already resolved; it
/// is applied at the ENTRY NODE (`entry`) and nowhere else. Keying it on the
/// node identity rather than on "the first `Stage` descended" is what lets the
/// entry sit inside a `Bind`/`Reduce`/`Foreach` SOURCE, whose siblings (`init`,
/// the body) may be evaluated first, and what makes a re-evaluated source (a
/// multi-output fold init) skip the same prefix every time. State is entirely
/// per-run, so several executors share no mutable state.
pub(crate) struct GraphMachine<'program, 'source> {
    pub(crate) nodes: &'program [ProgramNode],
    pub(crate) pending: Option<GraphTask<'source>>,
    pub(crate) frames: Vec<GraphFrame<'program, 'source>>,
    pub(crate) entry: ProgramNodeId,
    pub(crate) entry_offset: usize,
    /// The number of binder-occurrence slots the compiled program declared. The
    /// env vector is sized to exactly this ONCE, on the first slot write; a
    /// binder-free program has `slots == 0` and never grows it at all.
    pub(crate) slots: u32,
    /// The variable environment: one bound RESULT per binder OCCURRENCE, written
    /// by `Bind`/`Reduce`/`Foreach` and read by `Variable`-start stages and
    /// `.[$x]` steps. Slots are never reused across sibling scopes, so nothing
    /// saves or restores them — an unwinding frame simply drops.
    ///
    /// A slot holds the SAME representation the binder's source emitted: a
    /// document-backed source binds its located handle zero-copy (a re-borrow of
    /// the authoritative document, no bytes copied and no materializer workspace
    /// touched), and a computed source binds its owned value. Both halves navigate
    /// natively — located through [`descend`], owned through [`descend_borrowed`].
    pub(crate) env: Vec<EngineResult<'source>>,
    /// The run's reusable materializer workspace: document-independent
    /// cycle-detection scratch grown to the input's node count on the first
    /// located capture and reused across every capture, so the barrier pays the
    /// O(node-count) bitmap setup once per run rather than once per value.
    pub(crate) workspace: Option<MaterializeWorkspace>,
    /// One-entry memo of the last located-to-owned CAPTURE materialization,
    /// keyed by the located value's node handle.
    ///
    /// An answering builtin (`index`, `contains`, `has`, ...) materializes its
    /// whole subject per call, and a map body probing a bound array (`$b |
    /// index($v)`) hands the SAME located node to every call — without the
    /// memo that is one O(subject) deep materialization per probe. The memo
    /// turns the run into one materialization plus O(1) share-returns, which
    /// is sound because a document is immutable: the same (document, node)
    /// always materializes to the same owned value, and the memo is cleared on
    /// every `reseed`, so a memo handle can never outlive the run that made it.
    pub(crate) memo: Option<(NodeHandle, Value)>,
    /// The fact-write deltas this run recorded (`ProgramNode::FactAssign`).
    /// Empty for every program without a fact assignment; the edit lane
    /// drains it after the run to splice retained source, and query-time
    /// reads overlay it so a later `.@comment` sees the write.
    pub(crate) fact_deltas: Vec<FactDelta>,
    /// The seed's located document, kept only when the program contains a
    /// `FactAssign`. A value write emits an owned result, so a later fact write
    /// walks this origin for the span authority instead of requiring the pipe
    /// input to stay located.
    pub(crate) origin: Option<LocatedProduct<'source>>,
    /// The out-continuation of the value whose evaluation most recently RAISED a
    /// catch-eligible error — the routing position the failed value would have
    /// taken. A `try` barrier catches the raise only when its own out-continuation
    /// is at or below this (`out_cont <= raise_cont`), so an error DOWNSTREAM of a
    /// `try` (a piped/postfix consumer of its output, routing below the barrier)
    /// escapes it (`try .a | .b` and `(.a)?.b` error at `.b`).
    pub(crate) raise_cont: usize,
    /// The recursive-definition call depth this machine evaluates at: 0 for
    /// the root program drive, one more than its driver's for a callable body
    /// drive. [`Self::eval_call_def`] refuses a non-tail call at or above
    /// [`CALLABLE_DEPTH_LIMIT`], which bounds the nested-machine recursion
    /// exactly as the path-mode callable depth bounded the nested path runs
    /// it replaces.
    pub(crate) callable_depth: usize,
    /// Trampoline rounds completed since this machine was seeded — the
    /// divergence detector: a tail self-call runs flat and never grows
    /// [`Self::callable_depth`], so a genuinely divergent recursion would loop
    /// forever without this counter raising at [`TAIL_ROUND_LIMIT`].
    pub(crate) tail_rounds: u64,
    /// The compile-time correlated-scan table (see [`crate::analysis::join`]).
    /// Empty for almost every program, and the emptiness is what the per-node
    /// dispatch tests before anything else.
    pub(crate) scans: &'program [CorrelatedScan],
    /// The compile-time partial-sort table (see [`crate::analysis::topk`]):
    /// the `sort`/`sort_by` calls whose sole consumer is a constant-bound
    /// head/tail slice, answered with the bounded-heap partial sort. Empty for
    /// almost every program, and the emptiness is what the per-node dispatch
    /// tests before anything else.
    pub(crate) topk: &'program [PartialSort],
    /// The compile-time anti-join table (see [`crate::analysis::join`]):
    /// the `select` calls whose negated-containment predicate is answered from
    /// the sorted keyed index queried for the complement. Empty for almost every
    /// program.
    pub(crate) anti: &'program [AntiJoinScan],
    /// One index slot per row of `scans` PLUS one per row of `anti`, allocated
    /// on the first offer. A program with no rows never allocates it at all.
    pub(crate) joins: Vec<JoinSlot>,

    /// The USER-declared indexes, built at each
    /// declaration's evaluation and queried by every later recognized scan over
    /// the same (container node, key path). Machine-lifetime: one run of one
    /// program, so a declaration in an earlier comma/pipe arm is queried by a
    /// later one. Cleared on [`Self::reseed`] — a new element is a new
    /// document, whose node handles the stored indexes know nothing about.
    pub(crate) user_indexes: Vec<UserIndexSlot>,

    /// The run-cached whole-document fact state behind `json_facts`
    /// (`facts::JsonFactsCache`): the attached-fact drain and text-leaf scan
    /// built ONCE per document and reused by every later located call over the
    /// SAME document, instead of O(facts + nodes) per element. Machine-lifetime
    /// like [`Self::user_indexes`] and cleared on [`Self::reseed`] for the same
    /// reason — a new element may be a new document.
    ///
    /// Residency releases exactly like [`Self::user_indexes`]: the drained
    /// facts, owner grouping, and text-leaf set are PLAIN OWNED allocations
    /// holding no ledger charge, so [`Self::reseed`]'s
    /// `json_facts_cache.clear()` drops them at reseed and the struct-field
    /// drop releases whatever a run leaves at machine teardown.
    pub(crate) json_facts_cache: jqf_builtins::registry::builtins::facts::JsonFactsCache,

    /// The ENGINE-cursor store: one slot per `as ~x` binding
    /// OCCURRENCE, each holding the suspended cursor machine that binding
    /// created — or `None` before its bind evaluated, after its body ended
    /// (RAII release), and in nested evaluators that own no cursor. The vector
    /// GROWS ON DEMAND to the first binding's slot, so a program with no
    /// engine bindings never allocates it at all; a cursor is a separately
    /// owned suspended machine, so a `limit`/`first` cut or a raise in THIS
    /// machine cannot unwind or truncate it (the pinned persistent-cursor
    /// semantics), and dropping it releases its ledger charges like any other
    /// machine.
    pub(crate) cursors: Vec<Option<Box<GraphMachine<'program, 'source>>>>,

    /// The path-provenance register (Law 1), active while this machine
    /// evaluates a path-family body; a nested machine carries a CLONE the
    /// parent adopts at each item it routes.
    pub(crate) path: Option<path_register::PathRegister>,

    pub(crate) done: bool,

    /// One reusable nested machine for frozen argument evaluation. Reseeded
    /// per argument instead of allocated and dropped each time; the same
    /// `reuse`/`reseed` pattern as [`GraphFrame::PathWalkEmit`].
    pub(crate) arg_reuse: Option<Box<GraphMachine<'program, 'source>>>,

    /// One over-produced output held back by the callable-body drive's
    /// completion drain (route.rs): when the drive tries to discover a body's
    /// completion one admission early, a MULTI-output body answers with its
    /// next item instead of Complete. That item is parked HERE and published
    /// on the frame's next resumption, so the one-output-per-resumption
    /// protocol (and the countdown cuts that rely on it) stay intact. Cleared
    /// on [`Self::reseed`].
    pub(crate) queued_output: Option<EngineResult<'source>>,

    /// Cumulative frame-task transitions admitted this run. Counted beside
    /// every cooperative work admission when the run carries an iteration
    /// ceiling (`--max-iterations`); untouched (and free) otherwise. Reset on
    /// [`Self::reseed`], so a pooled nested machine starts its own run's
    /// budget fresh; the CAP itself is inherited from the seeding machine and
    /// survives reseed.
    pub(crate) iterations_done: u64,
    /// The run's opt-in transition ceiling; `None` is unlimited (the default).
    pub(crate) iteration_cap: Option<u64>,

    /// One reusable nested machine for recursive-callable BODY evaluation,
    /// one level down from [`Self::arg_reuse`]: reseeded per non-tail call
    /// admission instead of boxed-seeded and dropped per call (the route.rs
    /// `CallableBody` drive). A nested recursion holds at most one dead child
    /// per live ancestor, bounded by the callable-depth ceiling.
    pub(crate) body_reuse: Option<Box<GraphMachine<'program, 'source>>>,

    /// Frame-vector scratch for inlining a binder-free argument on this
    /// machine: swapped with [`Self::frames`] for the argument drive so the
    /// caller's continuation stack is untouched, then swapped back. Capacity
    /// survives across calls.
    pub(crate) arg_frames: Vec<GraphFrame<'program, 'source>>,

    /// Reusable [`Self::path_slots`] snapshot buffer. Taken for the call that
    /// needs it and recycled when the overlay vec is dropped.
    pub(crate) slot_scratch: Vec<Option<Value>>,

    /// Cached slot set the current tail-callable body reads, keyed by body
    /// id so a trampoline round does not re-walk the arena.
    pub(crate) tail_read_body: Option<ProgramNodeId>,
    pub(crate) tail_read_slots: Vec<VarSlot>,

    /// Filter-parameter closures currently bound on this machine. Empty for
    /// every program without a recursive filter-parameter def; grown on
    /// demand to the highest bound slot.
    pub(crate) filter_env: Vec<Option<FilterClosure>>,
}

impl core::fmt::Debug for GraphMachine<'_, '_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Deliberately shallow (profiler hygiene): shape, not contents.
        formatter
            .debug_struct("GraphMachine")
            .field("nodes", &self.nodes.len())
            .field("pending", &self.pending.is_some())
            .field("frames", &self.frames.len())
            .field("done", &self.done)
            .finish()
    }
}

/// One pipeline stage, classified when the walk opens.
#[derive(Clone, Copy, Debug)]
pub(crate) enum WalkStage {
    /// A kind filter, optionally wrapped in `select`.
    Kind {
        filter: jqf_builtins::registry::builtins::kinds::KindFilter,
        through_select: bool,
    },
    /// Needs a graph machine; the node is the stage root.
    Machine(ProgramNodeId),
}

/// One step of the resumable walk producer's advance, decided while the walker
/// is borrowed and executed after the borrow ends.
pub(crate) enum WalkStep<'source> {
    /// Publish the walker's register path.
    Publish(Value, usize),
    /// Seed the first pipeline stage over the walked child.
    SeedChild(EngineResult<'source>),
    /// The walk is exhausted; pop the frame.
    Exhausted,
    /// The root of a `paths` walk; continue.
    Skip,
}

/// One fact-write result: the located node whose attached fact changed, the
/// value path that names it, and its NEW payload. The edit lane applies the
/// delta as a span operation against the retained source and
/// re-verifies it by re-decoding and reading the fact at `path`.
#[derive(Clone, Debug)]
pub struct FactDelta {
    /// The value path (`["port"]`) naming the written node.
    pub path: Value,
    /// The written node in the ORIGINAL document (the span authority).
    pub node: NodeId,
    /// The fact role selector ("comment", "attribute").
    pub role: String,
    /// The fact kind: the attribute selector ("href") for a `.&href` write,
    /// empty for the comment roles.
    pub kind: String,
    /// The new payload as a semantic value (`null` or an empty list means
    /// delete; a list of text lines replaces a comment; a string replaces an
    /// attribute).
    pub payload: Value,
}

/// Re-locates one selected element node in the retained authority of a
/// located selector result, materializing it when the decode deferred the
/// subtree to a container span (the same law `retain_or_materialize` keeps
/// at the stage walk's entry points).
/// One selector result the [`GraphFrame::SelectorEmit`] producer publishes:
/// an element node id re-located in the frame's retained authority, or an
/// owned scalar from a top-level `xpath/1` function call. `Value` is not
/// `Clone`, so the producer consumes each result once from the front.
pub(crate) enum SelectorEmitValue {
    Node(jqf_data::NodeId),
    Owned(Value),
}

pub(crate) fn selector_located_node<'source>(
    located: &LocatedProduct<'source>,
    node: jqf_data::NodeId,
    executor: &mut GraphMachine<'_, 'source>,
    resources: &mut ResourceContext<'_>,
) -> Result<EngineResult<'source>, EngineRunError> {
    let product = located.product();
    let document = product.document();
    let handle = document
        .node_handle(node)
        .map_err(|_| internal_contract("selector node resolution over a valid document"))?;
    let result = EngineResult::Located(LocatedProduct::try_new(product, handle).map_err(EngineRunError::Codec)?);
    // A deferred container span must be materialized before it is walked.
    if document.container_span_count() > 0
        && document
            .value_view(handle)
            .map_err(|_| internal_contract("selector span view over a valid document"))?
            .is_container_span()
            .map_err(|_| internal_contract("selector span view over a valid document"))?
    {
        let owned = executor.materialize_capture(result, resources)?;
        return Ok(EngineResult::owned(owned));
    }
    Ok(result)
}

/// Builds an internal-contract [`EngineRunError`] for a broken navigation
/// invariant surfaced by the graph machine.
pub(crate) fn internal_contract(contract: &'static str) -> EngineRunError {
    EngineRunError::Codec(CodecError::new(CodecFailureKind::InternalContractViolation {
        contract,
    }))
}

/// Last-write-wins overlay of this run's [`FactDelta`]s over a node's
/// attached-fact read. A matching write hides the document's original fact;
/// an empty list or `null` payload is a delete (`null`).
fn overlay_attached_fact<'a>(
    deltas: &'a [FactDelta],
    node: NodeId,
    selector: &str,
    attribute: bool,
) -> Option<&'a Value> {
    deltas.iter().rev().find_map(|delta| {
        if delta.node != node {
            return None;
        }
        let matches = if attribute {
            delta.role == jqf_codec_core::markup::ATTRIBUTE_FACT && delta.kind == selector
        } else {
            accessor_matches_fact(&delta.role, selector)
        };
        matches.then_some(&delta.payload)
    })
}

pub(crate) fn overlay_read_payload(payload: &Value) -> Value {
    match payload {
        Value::Null => Value::Null,
        Value::Array(items) if items.is_empty() => Value::Null,
        other => other.clone(),
    }
}

/// Whether a `.@` selector names the recovered attribute-map projection.
fn accessor_role_ids(
    document: &jqf_data::Document<'_>,
    selector: &str,
    attribute: bool,
) -> Vec<jqf_data::FactRoleBindingId> {
    if attribute {
        document
            .fact_role_binding(jqf_codec_core::markup::ATTRIBUTE_FACT)
            .into_iter()
            .collect()
    } else {
        document
            .interned_fact_roles()
            .filter(|(_, name)| accessor_matches_fact(name, selector))
            .map(|(id, _)| id)
            .collect()
    }
}

fn attrs_map_selector(selector: &str) -> bool {
    accessor_matches_fact(jqf_codec_core::markup::ATTRS_FACT, selector)
}

/// One stored attribute as (name, payload), including HTML names that are
/// not jqf-data identities (map payload with `name`/`value`).
fn attribute_name_and_payload(fact: jqf_data::DocumentFact<'_>) -> Option<(String, Value)> {
    if fact.role().as_str() != jqf_codec_core::markup::ATTRIBUTE_FACT {
        return None;
    }
    match fact.payload() {
        jqf_data::FactPayloadView::Text(_) => {
            let payload = materialize_fact_payload(fact.payload()).ok()?;
            Some((String::from(fact.kind().as_str()), payload))
        }
        jqf_data::FactPayloadView::Map(map) => {
            let mut name = None;
            let mut value = None;
            for (key, payload) in map.iter() {
                match (key, payload) {
                    ("name", jqf_data::FactPayloadView::Text(text)) => {
                        name = Some(String::from(text));
                    }
                    ("value", jqf_data::FactPayloadView::Text(text)) => {
                        value = Some(String::from(text));
                    }
                    _ => {}
                }
            }
            let value = Value::try_string(&value?).ok()?;
            Some((name?, value))
        }
        _ => None,
    }
}

fn insert_attr_entry(builder: &mut ObjectBuilder, name: &str, payload: Value) -> Result<(), EngineRunError> {
    let key = jqf_data::ObjectKey::try_from_str(name).map_err(|_| EngineRunError::allocation_failure())?;
    builder
        .try_insert_or_replace(key, payload)
        .map_err(|_| EngineRunError::allocation_failure())?;
    Ok(())
}

fn apply_attr_entries(
    entries: Vec<(String, Value)>,
    node: NodeId,
    deltas: &[FactDelta],
) -> Result<Value, EngineRunError> {
    let mut builder = ObjectBuilder::new();
    let mut seen = alloc::collections::BTreeSet::<String>::new();
    for (name, payload) in entries {
        if let Some(overlay) = overlay_attached_fact(deltas, node, &name, true) {
            seen.insert(name.clone());
            let overlay = overlay_read_payload(overlay);
            if matches!(overlay, Value::Null) {
                continue;
            }
            insert_attr_entry(&mut builder, &name, overlay)?;
        } else {
            seen.insert(name.clone());
            insert_attr_entry(&mut builder, &name, payload)?;
        }
    }
    for delta in deltas {
        if delta.node != node || delta.role != jqf_codec_core::markup::ATTRIBUTE_FACT || seen.contains(&delta.kind) {
            continue;
        }
        let overlay = overlay_read_payload(&delta.payload);
        if matches!(overlay, Value::Null) {
            continue;
        }
        insert_attr_entry(&mut builder, &delta.kind, overlay)?;
    }
    builder
        .try_finish()
        .map(Value::Object)
        .map_err(|_| EngineRunError::allocation_failure())
}

/// Whether an engine fact-role selector is one of the three COMMENT
/// positions of the shared vocabulary ([`jqf_codec_core::comment`]: `comment`,
/// `comment_inline`, `comment_foot`). The comment payloads are line LISTS (the
/// read side round-trips them that way), so the write normalization splits on
/// line boundaries for exactly these roles; every other role (the markup
/// `attribute`) carries a single text payload and passes through.
pub(crate) fn is_comment_role(role: &str) -> bool {
    matches!(
        role,
        jqf_codec_core::comment::HEAD | jqf_codec_core::comment::INLINE | jqf_codec_core::comment::FOOT
    )
}

impl<'program, 'source> GraphMachine<'program, 'source> {
    /// Frame the emission walk already classified. The walk's match is the
    /// kind test; this is an index into that same slot, not a second
    /// classification.
    #[inline]
    pub(crate) fn routed_frame(&self, index: usize) -> &GraphFrame<'program, 'source> {
        &self.frames.as_slice()[index]
    }

    /// Mutable twin of [`Self::routed_frame`].
    #[inline]
    pub(crate) fn routed_frame_mut(&mut self, index: usize) -> &mut GraphFrame<'program, 'source> {
        &mut self.frames.as_mut_slice()[index]
    }
}

/// The DEFINED error for a pull that reached an evaluator owning no cursor
/// store (defense in depth): the reachable cases — a pull inside a
/// recursive `def` body, or inside a `~generator` argument — are rejected at
/// lower time, so this covers nested `paths`/`path` body machines and
/// path-mode positions. Catch-eligible like a program-raised value; the
/// message names the restriction rather than failing silently.
pub(crate) fn engine_binding_unavailable(_resources: &ResourceContext<'_>) -> EngineRunError {
    Value::try_string(
        "an engine binding cannot be pulled in this position (its cursor is \
         owned by another evaluator)",
    )
    .map_or_else(
        |_| EngineRunError::Codec(CodecError::new(CodecFailureKind::AllocationFailure)),
        EngineRunError::Raised,
    )
}

/// Engagements of the keyed static fast lane (bare `Key`/`Index` stage key
/// graphs), counted so tests can pin that the lane engages for static shapes
/// and stays silent for general key graphs.
pub(crate) static KEYED_STATIC_ENGAGEMENTS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Per-run scratch the step walk borrows when a step must MATERIALIZE located
/// document bytes into an owned value.
///
/// Its only caller is the `Slice` step: a slice's output is semantically a NEW
/// array (the law), so a located subrange clones out element by element. The
/// reusable workspace is what keeps that honest — a subrange pays the
/// O(node-count) cycle-detection bitmap ONCE per run rather than once per element.
/// Every other step (`Key`, `Index`, `Each`, `DynVar`, `Descend`) navigates
/// without touching it.
pub(crate) struct StepScratch<'machine, 'resources> {
    workspace: &'machine mut Option<MaterializeWorkspace>,
    resources: &'machine mut ResourceContext<'resources>,
    /// Fact writes recorded earlier in this run. Last-write-wins over the
    /// document's attached facts so `.a.@comment = ["x"] | .a.@comment` reads
    /// the payload just written. Empty for every program without a fact
    /// assignment, and for the bare-stage machine (which never hosts one).
    deltas: &'machine [FactDelta],
}

impl<'machine, 'resources> StepScratch<'machine, 'resources> {
    /// Borrows one machine's materializer scratch and ledger.
    pub(crate) fn new(
        workspace: &'machine mut Option<MaterializeWorkspace>,
        resources: &'machine mut ResourceContext<'resources>,
    ) -> Self {
        Self {
            workspace,
            resources,
            deltas: &[],
        }
    }

    /// Overlay this run's fact writes onto attached-fact reads.
    pub(crate) fn with_deltas(mut self, deltas: &'machine [FactDelta]) -> Self {
        self.deltas = deltas;
        self
    }

    /// The last-write-wins TAG delta recorded against `node` this run, when a
    /// fact write addressed the tag role. The `.@tag` read consults it BEFORE
    /// the node's intrinsic tag, so `.p.@tag = "!m" | .p.@tag` answers the
    /// payload just written — the same overlay every sibling role reads
    /// through.
    pub(crate) fn tag_delta(&self, node: jqf_data::NodeId) -> Option<&Value> {
        overlay_attached_fact(self.deltas, node, "tag", false)
    }

    /// Borrows the request ledger a step needs to buy an owned allocation.
    pub(crate) fn resources(&self) -> &ResourceContext<'resources> {
        self.resources
    }

    /// Borrows the request ledger MUTABLY (an attached-fact read accounts its
    /// batches against the ledger).
    pub(crate) fn resources_mut(&mut self) -> &mut ResourceContext<'resources> {
        self.resources
    }

    /// Materializes one located node into an owned value.
    ///
    /// # Errors
    ///
    /// Returns an internal-contract error when materializing a valid located
    /// node fails.
    pub(crate) fn materialize(
        &mut self,
        document: &jqf_data::Document<'_>,
        node: jqf_data::NodeHandle,
    ) -> Result<Value, EngineRunError> {
        let workspace = self.workspace.get_or_insert_with(MaterializeWorkspace::new);
        document
            .materialize_node_with(workspace, node, self.resources)
            .map_err(|_| internal_contract("materialization of a located slice element failed"))
    }

    /// Reads the FIRST attached fact on `node` matching one `.@`/`.&` accessor,
    /// materializing its payload into an owned value.
    ///
    /// A `.@name` accessor matches the fact whose ROLE is `name`; a `.&name`
    /// accessor matches the fact whose ROLE is `attribute` AND whose KIND is
    /// `name`. Facts are globally ordered, so the first match in that order is
    /// the accessor's answer (a narrowing for this first slice: multiple facts
    /// with the same role are not fanned out). A node with no matching fact
    /// answers `null` — the tree's standing spelling for an absent fact.
    ///
    /// The read walks the document's fact reader because that is the public
    /// attached-fact read API; a future fact-index would make this an owner
    /// lookup, not a scan.
    ///
    /// # Errors
    ///
    /// Returns an internal-contract error when the attached-fact reader fails
    /// over an otherwise valid document, or an allocation failure when a
    /// payload cannot be materialized.
    pub(crate) fn read_attached_fact(
        &mut self,
        document: &jqf_data::Document<'_>,
        node: jqf_data::NodeHandle,
        selector: &str,
        attribute: bool,
    ) -> Result<Value, EngineRunError> {
        let owner = jqf_data::LocalOwnerRef::Node(
            document
                .resolve_node_handle(node)
                .map_err(|_| internal_contract("accessor node resolution"))?,
        );
        // Fast path: the finalize-time owner index names the owning node's
        // facts directly, so a fact-dense document's accessor reads O(facts
        // on this node) instead of scanning every fact.
        let jqf_data::LocalOwnerRef::Node(node_id) = owner else {
            return Ok(Value::Null);
        };
        if let Some(payload) = overlay_attached_fact(self.deltas, node_id, selector, attribute) {
            return Ok(overlay_read_payload(payload));
        }
        let roles = accessor_role_ids(document, selector, attribute);
        let kind = if attribute {
            document.fact_kind_binding(selector)
        } else {
            None
        };
        if attribute && (roles.is_empty() || kind.is_none()) {
            return Ok(Value::Null);
        }
        if document.fact_owner_indexed() {
            if !roles.is_empty() {
                match document.owner_fact_payload_in(node_id, &roles, kind) {
                    Ok(Some(view)) => return materialize_fact_payload(view),
                    Ok(None) => {}
                    Err(_) => return Err(internal_contract("attached-fact owner lookup failed")),
                }
            }
            if !attribute && attrs_map_selector(selector) {
                return self.read_attrs_map(document, node_id);
            }
            return Ok(Value::Null);
        }
        // Fallback: a document whose fact-owner index did not run scans
        // the whole fact arena.
        let limit = jqf_data::BatchLimit::new(usize::MAX).ok_or_else(|| internal_contract("fact batch limit"))?;
        let mut reader = match document.fact_reader(self.resources) {
            Ok(reader) => reader,
            // A document that does not retain attached facts has none: the
            // accessor answers `null`, exactly as a document with the
            // capability but no matching fact does.
            Err(jqf_data::DataError::CapabilityUnavailable {
                capability: jqf_data::DocumentCapability::AttachedFacts,
            }) => return Ok(Value::Null),
            Err(_) => {
                return Err(internal_contract("attached-fact reader over a valid document"));
            }
        };
        // The reader does not retain the resource borrow: `poll_batch` releases
        // it before returning a batch, so a matched payload can be materialized
        // with the immutable borrow in the same iteration.
        loop {
            let poll = reader
                .poll_batch(limit, self.resources)
                .map_err(|_| internal_contract("attached-fact read failed over a valid document"))?;
            match poll {
                jqf_data::ReaderPoll::Batch(batch) => {
                    for fact in batch.iter() {
                        if fact.owner() != owner {
                            continue;
                        }
                        if roles.iter().any(|role| fact.role_binding() == *role)
                            && kind.is_none_or(|kind| fact.kind_binding() == kind)
                        {
                            return materialize_fact_payload(fact.payload());
                        }
                    }
                }
                jqf_data::ReaderPoll::Pending => {
                    // The cooperative entry's work credits are exhausted;
                    // refill so the scan can continue — spinning without
                    // refilling would loop forever once the per-entry budget
                    // is gone.
                    self.resources_mut()
                        .try_begin_next_cooperative_entry(4096)
                        .map_err(|error| EngineRunError::Codec(CodecError::from(error)))?;
                }
                jqf_data::ReaderPoll::End(_) => break,
            }
        }
        if !attribute && attrs_map_selector(selector) {
            return self.read_attrs_map(document, node_id);
        }
        Ok(Value::Null)
    }

    /// `.@attrs` from the per-attribute table (the stored map fact is gone).
    fn read_attrs_map(&mut self, document: &jqf_data::Document<'_>, node_id: NodeId) -> Result<Value, EngineRunError> {
        let mut entries = Vec::new();
        let mut is_element = false;
        if document.fact_owner_indexed() {
            for fact_id in document.owner_fact_ids(node_id) {
                let fact = document
                    .fact(*fact_id)
                    .map_err(|_| internal_contract("indexed fact resolution"))?;
                if accessor_matches_fact(fact.role().as_str(), "name") {
                    is_element = true;
                }
                if let Some(pair) = attribute_name_and_payload(fact) {
                    is_element = true;
                    entries.push(pair);
                }
            }
        } else {
            let owner = jqf_data::LocalOwnerRef::Node(node_id);
            let limit = jqf_data::BatchLimit::new(usize::MAX).ok_or_else(|| internal_contract("fact batch limit"))?;
            let mut reader = match document.fact_reader(self.resources) {
                Ok(reader) => reader,
                Err(jqf_data::DataError::CapabilityUnavailable {
                    capability: jqf_data::DocumentCapability::AttachedFacts,
                }) => return Ok(Value::Null),
                Err(_) => {
                    return Err(internal_contract("attached-fact reader over a valid document"));
                }
            };
            loop {
                let poll = reader
                    .poll_batch(limit, self.resources)
                    .map_err(|_| internal_contract("attached-fact read failed over a valid document"))?;
                match poll {
                    jqf_data::ReaderPoll::Batch(batch) => {
                        for fact in batch.iter() {
                            if fact.owner() != owner {
                                continue;
                            }
                            if accessor_matches_fact(fact.role().as_str(), "name") {
                                is_element = true;
                            }
                            if let Some(pair) = attribute_name_and_payload(fact) {
                                is_element = true;
                                entries.push(pair);
                            }
                        }
                    }
                    jqf_data::ReaderPoll::Pending => {
                        self.resources_mut()
                            .try_begin_next_cooperative_entry(4096)
                            .map_err(|error| EngineRunError::Codec(CodecError::from(error)))?;
                    }
                    jqf_data::ReaderPoll::End(_) => break,
                }
            }
        }
        let overlay_attrs = self
            .deltas
            .iter()
            .any(|delta| delta.node == node_id && delta.role == jqf_codec_core::markup::ATTRIBUTE_FACT);
        if !is_element && !overlay_attrs {
            return Ok(Value::Null);
        }
        apply_attr_entries(entries, node_id, self.deltas)
    }
}

/// Takes one Working reservation for a batch of accumulated key bytes,
/// reporting whether the ledger admitted it.
///
/// A refusal is a DECLINE, never an error: the naive scan needs none of these
/// bytes, so a program that fits without an index must still run.
/// The half-open range of index entries whose key is `probe`.
///
/// `partition_point` over a `total_cmp`-sorted slice, twice: the first call
/// finds where the equal run starts, the second where it ends. The entries are
/// ordered by the very function `==` is defined as, so "equal under the index"
/// and "equal under the operator" are the same predicate by construction.
///
/// A comparison that runs past its depth ceiling cannot be bisected on, and
/// the bisection READS only O(log n) entries — so after it converges, every
/// entry of the reported window is re-compared and ANY capped key inside the
/// run declines the whole answer. An entry outside the window is not
/// re-read; that is the price of keeping the probe O(log n), and the index
/// build already declined any key pair its own sort saw trip the cap.
///
/// `None` is a DECLINE, not an error: a probe that compares too deep against
/// some key cannot be bisected, so the scan falls back to the naive form and
/// lets `==` raise in its own position with its own message.
pub(crate) fn equal_key_run(entries: &[JoinEntry], probe: &Value) -> Option<(usize, usize)> {
    let mut visited_capped = false;
    let start = entries.partition_point(|entry| {
        order::total_cmp(&entry.key, probe)
            .unwrap_or_else(|_| {
                visited_capped = true;
                core::cmp::Ordering::Equal
            })
            .is_lt()
    });
    let end = start
        + entries[start..].partition_point(|entry| {
            !order::total_cmp(&entry.key, probe)
                .unwrap_or_else(|_| {
                    visited_capped = true;
                    core::cmp::Ordering::Equal
                })
                .is_gt()
        });
    // A cap the bisection itself saw is enough to decline.
    if visited_capped {
        return None;
    }
    // The window the fold built may hold entries the search never read;
    // re-compared under the plain law, any one of them declines.
    for entry in &entries[start..end] {
        if order::total_cmp(&entry.key, probe).is_err() {
            return None;
        }
    }
    Some((start, end))
}

/// The `position`-th child of a keyed collection's subject, retained.
///
/// An array's children are its elements and an OBJECT's are its values, which is
/// what makes `{"a":1} | sort_by(.)` collect a key at all before failing.
///
/// Shared with [`pathmode`], whose keyed arm collapses the same drive into one
/// frozen key run per child.
pub(crate) fn keyed_child(subject: &Value, position: usize) -> Result<Value, EngineRunError> {
    let child = match subject.untagged() {
        Value::Array(array) => array.get(position),
        Value::Object(object) => object.get_index(position).map(jqf_data::ObjectEntry::value),
        _ => return Err(internal_contract("a keyed drive over a non-container")),
    };
    Ok(child
        .ok_or_else(|| internal_contract("a keyed cursor past the subject's last child"))?
        .clone())
}

/// The key a [`GraphFrame::Walk`]'s object arm files its next output under.
pub(crate) fn walk_key(subject: &Value, position: usize) -> Result<jqf_data::ObjectKey, EngineRunError> {
    let Value::Object(object) = subject.untagged() else {
        return Err(internal_contract("a walk object arm over a non-object"));
    };
    let entry = object
        .get_index(position)
        .ok_or_else(|| internal_contract("a walk cursor past the subject's last member"))?;
    semantics_path::clone_key(entry.key())
}

/// Publishes a completed keyed collection, or rejects a subject that iterated
/// without being an array.
///
/// The rejection is the one place the engine RENDERS A VALUE IT NEVER BUILT:
/// the two-operand implementation is reached through `map([f])`, so its message
/// names the boxed key array, and reproducing the text means fabricating that
/// array here from the entries the frame did collect. Which of the two doubled
/// templates it takes is [`jqf_builtins::semantics::keyed::KeyModeRow::rejection`]'s
/// law, not this function's.
pub(crate) fn finish_keyed(
    mode: jqf_builtins::semantics::keyed::KeyMode,
    subject: &Value,
    entries: &mut [jqf_builtins::semantics::keyed::KeyedEntry],
    spill_runs: &[jqf_resource::RunId],
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    use jqf_builtins::semantics::keyed::{Rejection, publish, row};
    if let Value::Array(array) = subject.untagged() {
        return publish(mode, entries, array, spill_runs, resources);
    }
    // The rejection's RIGHT operand is the `map([f])` result — one BOXED key
    // per entry, the empty box for an empty output — so the unboxed `One`
    // entries are re-boxed here. This is an error path: the allocations it
    // makes are the price of the message, never a hot-row cost.
    let mut boxed = Array::try_with_capacity(entries.len()).map_err(|_| EngineRunError::allocation_failure())?;
    for entry in entries.iter() {
        let key = match &entry.key {
            jqf_builtins::semantics::keyed::KeyValue::Empty => {
                Value::Array(Array::try_new().map_err(|_| EngineRunError::allocation_failure())?)
            }
            jqf_builtins::semantics::keyed::KeyValue::One(value) => {
                let mut inner = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
                inner
                    .try_push(value.clone())
                    .map_err(|_| EngineRunError::allocation_failure())?;
                Value::Array(inner)
            }
            jqf_builtins::semantics::keyed::KeyValue::Many(array) => Value::Array(array.clone_shared()),
            jqf_builtins::semantics::keyed::KeyValue::Bare(value) => value.clone(),
        };
        boxed.try_push(key).map_err(|_| EngineRunError::allocation_failure())?;
    }
    let keys = Value::Array(boxed);
    let left = message::dump_trunc_owned(subject)?;
    let right = message::dump_trunc_owned(&keys)?;
    let text = match row(mode).rejection {
        Rejection::NotBothArrays => message::not_both_arrays_message(subject.kind(), &left, keys.kind(), &right)?,
        Rejection::NotIterable => message::not_iterable_pair_message(subject.kind(), &left, keys.kind(), &right)?,
    };
    Err(jqf_builtins::semantics::path::raise(&text, resources))
}

/// Whether one step is a static object-key or array-index step (the declared
/// index's admitted grammar — the recognizer's row shape).
pub(crate) fn is_static_key_step(step: &StageStep) -> bool {
    matches!(step.access(), StepAccess::Key(_) | StepAccess::Index(_))
}

/// The canonical [`UserKeyStep`] form of a `Stage{Current, …}` node's steps,
/// or `None` for any node outside the declared key-path grammar: a non-`Stage`
/// node, a non-`Current` start, an optional step, or an `Each`/`Descend`/
/// dynamic step. Both the declaration and the scan-side match convert through
/// here, so the two sides compare the same canonical form.
pub(crate) fn user_key_steps(nodes: &[ProgramNode], stage: ProgramNodeId) -> Option<Vec<UserKeyStep>> {
    let ProgramNode::Stage { start, steps } = &nodes[stage.index()] else {
        return None;
    };
    if !matches!(start, StageStart::Current) {
        return None;
    }
    let mut out = Vec::new();
    for step in steps {
        if step.is_optional() {
            return None;
        }
        match step.access() {
            StepAccess::Key(key) => out.push(UserKeyStep::Key(key.clone())),
            StepAccess::Index(index) => out.push(UserKeyStep::Index(*index)),
            _ => return None,
        }
    }
    Some(out)
}

pub(crate) fn located_child_count(container: &Container<'_>) -> Result<Option<usize>, EngineRunError> {
    let Container::Located(located) = container else {
        return Ok(None);
    };
    let document = located.product().document();
    // The payload-transparent view: a tag LAYER is descended before the node is
    // asked its kind, so a tagged container's width is its payload's width —
    // the same law the owned keyed drive spells for `Value::Tagged`.
    let view = document
        .payload_view(located.node())
        .map_err(|_| internal_contract("join container view failed over a valid node"))?;
    let kind = view
        .kind()
        .map_err(|_| internal_contract("join container kind failed over a valid node"))?;
    match kind {
        ValueKind::Array => Ok(view
            .array()
            .map_err(|_| internal_contract("array node without an array projection"))?
            .map(jqf_data::ArrayView::len)),
        ValueKind::Object => Ok(view
            .object()
            .map_err(|_| internal_contract("object node without an object projection"))?
            .map(jqf_data::ObjectView::len)),
        _ => Ok(None),
    }
}

/// Reserves and commits `bytes` of Working residency, mapping a ledger rejection
/// onto the machine's resource channel.
/// One eager filter evaluation over an owned value, through the same
/// path-mode sub-run the path-family value laws use. The regex laws evaluate
/// their pattern/flags arguments this way, and `sub`/`gsub` evaluate their
/// replacement filter once per match with dot = the capture object.
/// The text of a compile-time literal STRING argument, if the argument node is
/// one. A literal replacement filter never reads the capture object, so
/// `sub`/`gsub` splice it without a per-match path-mode run.
pub(crate) fn literal_string_argument(nodes: &[ProgramNode], node: ProgramNodeId) -> Option<&str> {
    match &nodes[node.index()] {
        ProgramNode::Stage {
            start: StageStart::Literal(Value::String(text)),
            steps,
        } if steps.is_empty() => Some(text.as_str()),
        _ => None,
    }
}

/// A fallible per-element clone of a value vector (`Value` is not `Clone`).
pub(crate) fn clone_values(values: &[Value]) -> Result<Vec<Value>, EngineRunError> {
    let mut copied = Vec::new();
    copied
        .try_reserve_exact(values.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for value in values {
        copied.push(value.clone());
    }
    Ok(copied)
}

/// Right-outer Cartesian step: every existing combo extended by every
/// output. Used when a call argument yields more than one value.
pub(crate) fn cartesian_extend(combos: &[Vec<Value>], outputs: &[Value]) -> Result<Vec<Vec<Value>>, EngineRunError> {
    let mut next = Vec::new();
    let count = combos.len().saturating_mul(outputs.len());
    next.try_reserve(count)
        .map_err(|_| EngineRunError::allocation_failure())?;
    for combo in combos {
        for output in outputs {
            let mut extended = clone_values(combo)?;
            extended.push(output.clone());
            next.push(extended);
        }
    }
    Ok(next)
}

/// A fallible copy of the binder-slot overlay (a `Value` is fallible-clone
/// only, and every nested filter evaluation needs its own run's overlay).
pub(crate) fn clone_slots(slots: &[Option<Value>]) -> Vec<Option<Value>> {
    slots.to_vec()
}

/// Drives one regex law: evaluates its argument filters and computes every
/// output value, following the eager argument evaluation. The
/// two-argument forms combine flags OUTER (the evaluation order for
/// `test`/`match`/`capture`/`scan`/`splits`); `split/2` combines pattern
/// OUTER, matching the eager drive.
#[allow(
    clippy::too_many_lines,
    reason = "one arm per law arity: the drive IS the dispatch table, and splitting it would \
              hide which shapes are covered"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "the frozen argument-machine seed: program, resources, slots, law, args, topk, and fact deltas are each an independent channel the drive needs at once"
)]
pub(crate) fn regex_drive(
    nodes: &[ProgramNode],
    resources: &mut ResourceContext<'_>,
    root: &Value,
    slots: &[Option<Value>],
    law: regex_builtins::RegexLaw,
    args: &[ProgramNodeId],
    topk: &[PartialSort],
    fact_deltas: &mut Vec<FactDelta>,
) -> Result<(Vec<Value>, Option<EngineRunError>), EngineRunError> {
    let subject = root.clone();
    let mut out = Vec::new();
    let mut pending: Option<EngineRunError> = None;
    // Evaluates one argument with the PUBLISHED-PREFIX law: the outputs are
    // collected as they land, and an error mid-argument returns them beside
    // the error, so the caller's product loop can publish the combinations
    // already computed before raising — the behavior for `splits("X"; ("g",
    // error("z")))` and its kin.
    let mut collect = |node: ProgramNodeId| -> Result<(Vec<Value>, Option<EngineRunError>), EngineRunError> {
        path_register::evaluate_owned_filter_prefix(
            nodes,
            resources,
            clone_slots(slots),
            node,
            subject.clone(),
            topk,
            fact_deltas,
        )
    };
    match law {
        // A /1 form has NO flags argument: the `builtin.jq` definition spells the
        // absence as `null` (`def scan($re): scan($re; null);`), and each law
        // owns the default that stands in for it. The single argument's
        // outputs stream with the prefix-keep law.
        regex_builtins::RegexLaw::Test1
        | regex_builtins::RegexLaw::Match1
        | regex_builtins::RegexLaw::Capture1
        | regex_builtins::RegexLaw::Scan1
        | regex_builtins::RegexLaw::Splits1 => {
            let (patterns, argument_pending) = collect(args[0])?;
            for pattern in &patterns {
                out.extend(regex_builtins::apply_combo(
                    law,
                    &subject,
                    pattern,
                    &Value::Null,
                    resources,
                )?);
            }
            pending = argument_pending;
        }
        // The flags-OUTER family (test/match/capture/scan — the evaluation
        // order): the FIRST argument (the pattern) varies fastest,
        // so the pattern outputs stream with the prefix-keep law and the
        // flags are the outer loop. An error in either argument surfaces
        // after the combinations already computed.
        regex_builtins::RegexLaw::Test2 | regex_builtins::RegexLaw::Match2 | regex_builtins::RegexLaw::Capture2 => {
            let (patterns, mut patterns_pending) = collect(args[0])?;
            let (flags, flags_pending) = collect(args[1])?;
            'outer: for flag in &flags {
                for pattern in &patterns {
                    if let Err(error) = (|| {
                        out.extend(regex_builtins::apply_combo(law, &subject, pattern, flag, resources)?);
                        Ok::<(), EngineRunError>(())
                    })() {
                        pending = Some(error);
                        break 'outer;
                    }
                }
                // The pattern argument's error surfaces at the first flag's
                // pattern exhaustion.
                if let Some(error) = core::mem::take(&mut patterns_pending) {
                    pending = Some(error);
                    break;
                }
            }
            if pending.is_none() {
                pending = flags_pending.or(patterns_pending);
            }
        }
        // The patterns-OUTER family (split/splits and scan — the evaluation
        // order): the LAST argument (the flags)
        // varies fastest, so the flags stream with the prefix-keep law and
        // the patterns are the outer loop: `[scan("a","b";"g","gi")]`
        // answers `["a","a","A","b","b","B"]` — the pattern is the outer loop,
        // the same nesting its `sub`/`test` siblings use.
        regex_builtins::RegexLaw::Splits2 | regex_builtins::RegexLaw::Split2 | regex_builtins::RegexLaw::Scan2 => {
            let (patterns, patterns_pending) = collect(args[0])?;
            let (flags, mut flags_pending) = collect(args[1])?;
            'outer: for pattern in &patterns {
                for flag in &flags {
                    if let Err(error) = (|| {
                        out.extend(regex_builtins::apply_combo(law, &subject, pattern, flag, resources)?);
                        Ok::<(), EngineRunError>(())
                    })() {
                        pending = Some(error);
                        break 'outer;
                    }
                }
                if let Some(error) = core::mem::take(&mut flags_pending) {
                    pending = Some(error);
                    break;
                }
            }
            if pending.is_none() {
                pending = patterns_pending.or(flags_pending);
            }
        }
        // The substitution family (sub/gsub): the patterns are the outer
        // loop, the flags stream with the prefix-keep law, and the
        // REPLACEMENT is evaluated per MATCH inside the combo, as the
        // substitution law requires (an erroring replacement surfaces after
        // the combos already assembled).
        regex_builtins::RegexLaw::Sub2
        | regex_builtins::RegexLaw::Sub3
        | regex_builtins::RegexLaw::Gsub2
        | regex_builtins::RegexLaw::Gsub3 => {
            let (patterns, patterns_pending) = collect(args[0])?;
            let (flags_values, mut flags_pending) = if args.len() == 2 {
                (vec![Value::Null], None)
            } else {
                collect(args[2])?
            };
            let literal = literal_string_argument(nodes, args[1]);
            'outer: for pattern in &patterns {
                for flags in &flags_values {
                    if let Err(error) = (|| {
                        if let Some(replacement) = literal {
                            let spans =
                                regex_builtins::substitution_match_spans(law, &subject, pattern, flags, resources)?;
                            out.extend(regex_builtins::substitute_assembled_literal(
                                &subject,
                                &spans,
                                replacement,
                                resources,
                            )?);
                            return Ok::<(), EngineRunError>(());
                        }
                        let matches = regex_builtins::substitution_matches(law, &subject, pattern, flags, resources)?;
                        let mut replacements = Vec::new();
                        for matched in &matches {
                            let input = matched.captures.clone();
                            let values = path_register::evaluate_owned_filter(
                                nodes,
                                resources,
                                clone_slots(slots),
                                args[1],
                                input,
                                topk,
                                fact_deltas,
                            )?;
                            replacements.push(values);
                        }
                        out.extend(regex_builtins::substitute_assembled(
                            &subject,
                            &matches,
                            &replacements,
                            resources,
                        )?);
                        Ok::<(), EngineRunError>(())
                    })() {
                        pending = Some(error);
                        break 'outer;
                    }
                }
                if let Some(error) = core::mem::take(&mut flags_pending) {
                    pending = Some(error);
                    break;
                }
            }
            if pending.is_none() {
                pending = patterns_pending.or(flags_pending);
            }
        }
    }
    Ok((out, pending))
}

/// Copies an object-construction key into a shared object key.
///
/// The text is copied rather than shared out of the source string or document:
/// an object key's bytes are charged per ENTRY, so a key allocation may not be
/// handed to a holder that outlives the entries paying for it.
pub(crate) fn copy_key(key: &str) -> Result<jqf_data::ObjectKey, EngineRunError> {
    jqf_data::ObjectKey::try_from_str(key).map_err(|_| EngineRunError::allocation_failure())
}

/// Whether this value carries a NON-CORE intrinsic tag (a `Value::Tagged`
/// projection), as opposed to a resolved core tag or none at all.
///
/// The tag is invisible to `type` and the kind filters by design — tagged
/// navigation is payload-transparent — so a shortcut that must not fire on a
/// tagged value has to ask for the tag explicitly.
pub(crate) fn carries_non_core_tag(value: &EngineResult<'_>) -> Result<bool, EngineRunError> {
    match value {
        EngineResult::Owned(value) => Ok(matches!(value, Value::Tagged { .. })),
        EngineResult::Located(located) => Ok(located
            .product()
            .document()
            .value_view(located.node())
            .map_err(|_| internal_contract("tag lookup node view failed"))?
            .tag_semantics()
            .map_err(|_| internal_contract("tag lookup failed over a valid document"))?
            .is_some_and(|semantics| semantics == jqf_data::IntrinsicTagSemantics::Tagged)),
    }
}

/// Extracts an object construction key: a core string (a located `String` node or
/// an owned `Value::String`) yields its text; every other type — including a
/// tagged string, which is not silently unwrapped (AGENTS.md) — is the
/// object-key mismatch class.
pub(crate) fn extract_key_string(
    value: &EngineResult<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<jqf_data::ObjectKey, EngineRunError> {
    match value {
        EngineResult::Owned(Value::String(text)) => copy_key(text),
        EngineResult::Owned(other) => Err(EngineRunError::ObjectKeyMismatch {
            actual_type: owned_value_kind(other),
            operand: message::dump_trunc_owned(other)?,
        }),
        EngineResult::Located(located) => {
            let document = located.product().document();
            // A non-core intrinsic tag (a `Value::Tagged` projection) is not a
            // plain string key even when its payload is textual. The refusal is
            // decided on the OUTER node's tag semantics; the payload reads
            // (scalar, kind, operand dump) take the descended view, so a tag
            // LAYER renders the clean key mismatch rather than failing an
            // internal contract over a perfectly valid document.
            let tagged = document
                .value_view(located.node())
                .map_err(|_| internal_contract("key node view failed over a valid document"))?
                .tag_semantics()
                .map_err(|_| internal_contract("key node tag lookup failed"))?
                .is_some_and(|semantics| semantics == jqf_data::IntrinsicTagSemantics::Tagged);
            let view = document
                .payload_view(located.node())
                .map_err(|_| internal_contract("key node view failed over a valid document"))?;
            match view
                .scalar()
                .map_err(|_| internal_contract("key node scalar view failed"))?
            {
                Some(jqf_data::ScalarView::String(text)) if !tagged => copy_key(text),
                _ => {
                    let actual_type = view
                        .kind()
                        .map_err(|_| internal_contract("key node kind lookup failed"))?;
                    // The located operand render can hit a DEFERRED container
                    // span inside the key's subtree (the demand-projection
                    // frontier keeps a container span until a read forces it,
                    // and a span has no occurrence view). The error path is
                    // cold, so fall back to materializing the key node and
                    // rendering the owned value — same bytes, no span.
                    let operand = if let Ok(text) = message::dump_trunc_view(&view) {
                        text
                    } else {
                        let owned = document
                            .materialize_node(located.node(), resources)
                            .map_err(|_| internal_contract("key node materialization failed"))?;
                        message::dump_trunc_owned(&owned)?
                    };
                    Err(EngineRunError::ObjectKeyMismatch { actual_type, operand })
                }
            }
        }
    }
}

/// The payload-transparent semantic category of an owned value (for the object-key
/// mismatch message).
pub(crate) fn owned_value_kind(value: &Value) -> ValueKind {
    match value {
        Value::Null => ValueKind::Null,
        Value::Bool(_) => ValueKind::Bool,
        Value::String(_) => ValueKind::String,
        Value::Bytes(_) => ValueKind::Bytes,
        Value::Array(_) => ValueKind::Array,
        Value::Object(_) => ValueKind::Object,
        Value::LocalDate(_) => ValueKind::LocalDate,
        Value::LocalTime(_) => ValueKind::LocalTime,
        Value::LocalDateTime(_) => ValueKind::LocalDateTime,
        Value::OffsetDateTime(_) => ValueKind::OffsetDateTime,
        Value::Tagged { payload, .. } => owned_value_kind(payload),
        Value::Number(_) => ValueKind::Number,
    }
}

/// Hands a frame's RETAINED input on to the run it seeds: MOVED when the frame's
/// generator is spent, re-BORROWED while it can still emit.
///
/// The `Null` left behind is never read. A retaining frame reads its input only
/// on the NEXT consumption, which `spent` reports impossible, and every one of
/// them then simply pops at the top of the stack. Moving is what keeps a frame
/// out of an in-place accumulator's way: a second live handle turns each write
/// into a whole-container copy-on-write rebuild, O(n) per write and O(n²) over a
/// fold.
///
/// The ledger is untouched by the choice. A value's own bytes are charged on its
/// ALLOCATION and released by its last drop, and the frame's shadow-byte charge
/// is for the frame slot, not for what the slot holds — so a move and a borrow
/// reserve and release exactly the same bytes.
/// The value a step-less `Literal` stage produces, read off the arena instead of
/// evaluated: one output, no input read, no frame, no failure. Its borrow is of
/// the PROGRAM, not of the machine, so a pair can be applied against it directly.
pub(crate) fn constant_operand(nodes: &[ProgramNode], node: ProgramNodeId) -> Option<&Value> {
    match &nodes[node.index()] {
        ProgramNode::Stage {
            start: StageStart::Literal(literal),
            steps,
        } if steps.is_empty() => Some(literal),
        _ => None,
    }
}

/// Fills [`ObjectMemberNode::static_key`] when the key graph is a step-less
/// string literal. Conservative: anything else stays dynamic.
pub(crate) fn mark_static_object_keys(nodes: &mut [ProgramNode]) {
    let keys: Vec<Vec<Option<String>>> = nodes
        .iter()
        .map(|node| match node {
            ProgramNode::ConstructObject { members, .. } => members
                .iter()
                .map(|member| match constant_operand(nodes, member.key) {
                    Some(Value::String(text)) => Some(alloc::string::String::from(text.as_str())),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        })
        .collect();
    for (node, keys) in nodes.iter_mut().zip(keys) {
        if let ProgramNode::ConstructObject { members, .. } = node {
            for (member, key) in members.iter_mut().zip(keys) {
                member.static_key = key;
            }
        }
    }
}

/// Conservative: a graph whose every path is at most one output. `Each` and
/// `Descend` are many; an unlisted node family stays unproven.
pub(crate) fn graph_at_most_one(nodes: &[ProgramNode], node: ProgramNodeId) -> bool {
    match &nodes[node.index()] {
        ProgramNode::Stage { steps, .. } => steps.iter().all(|step| {
            !matches!(
                step.access(),
                crate::program::StepAccess::Each | crate::program::StepAccess::Descend
            )
        }),
        ProgramNode::Empty | ProgramNode::CollectArray { .. } | ProgramNode::CountCollect { .. } => true,
        // One final state PER INIT OUTPUT: a multi-output init starts one
        // complete fold each, so only a single-output init is at most one.
        ProgramNode::Reduce { init, .. } => graph_at_most_one(nodes, *init),
        ProgramNode::Binary { left, right, .. }
        | ProgramNode::FlatMap {
            upstream: left,
            body: right,
        }
        | ProgramNode::Alternative { left, right }
        | ProgramNode::Logical { left, right, .. }
        | ProgramNode::Bind {
            source: left,
            body: right,
            ..
        } => graph_at_most_one(nodes, *left) && graph_at_most_one(nodes, *right),
        ProgramNode::Concat { parts } => parts.iter().all(|part| graph_at_most_one(nodes, *part)),
        ProgramNode::Conditional {
            condition,
            consequent,
            alternative,
        } => {
            graph_at_most_one(nodes, *condition)
                && graph_at_most_one(nodes, *consequent)
                && graph_at_most_one(nodes, *alternative)
        }
        ProgramNode::Try { body, handler } => {
            graph_at_most_one(nodes, *body) && handler.is_none_or(|handler| graph_at_most_one(nodes, handler))
        }
        ProgramNode::ChainBody { body } => graph_at_most_one(nodes, *body),
        ProgramNode::ConstructObject { members, .. } => members
            .iter()
            .all(|member| graph_at_most_one(nodes, member.key) && graph_at_most_one(nodes, member.value)),
        _ => false,
    }
}

/// Whether `node`'s graph writes a binder slot. Inline argument evaluation
/// shares the caller's env, so a graph that writes slots is not eligible.
pub(crate) fn graph_binds_slots(nodes: &[ProgramNode], node: ProgramNodeId) -> bool {
    match &nodes[node.index()] {
        ProgramNode::Bind { .. }
        | ProgramNode::Reduce { .. }
        | ProgramNode::Foreach { .. }
        | ProgramNode::CallDef { .. }
        | ProgramNode::CallFilter { .. }
        | ProgramNode::EngineBind { .. } => true,
        ProgramNode::Stage { .. } | ProgramNode::Empty | ProgramNode::EnginePull { .. } | ProgramNode::Break { .. } => {
            false
        }
        ProgramNode::Binary { left, right, .. }
        | ProgramNode::FlatMap {
            upstream: left,
            body: right,
        }
        | ProgramNode::Choice { left, right }
        | ProgramNode::Alternative { left, right }
        | ProgramNode::Logical { left, right, .. } => {
            graph_binds_slots(nodes, *left) || graph_binds_slots(nodes, *right)
        }
        ProgramNode::CollectArray { body } | ProgramNode::CountCollect { body } => {
            body.is_some_and(|body| graph_binds_slots(nodes, body))
        }
        ProgramNode::Concat { parts } => parts.iter().any(|part| graph_binds_slots(nodes, *part)),
        ProgramNode::Conditional {
            condition,
            consequent,
            alternative,
        } => {
            graph_binds_slots(nodes, *condition)
                || graph_binds_slots(nodes, *consequent)
                || graph_binds_slots(nodes, *alternative)
        }
        ProgramNode::Try { body, handler } => {
            graph_binds_slots(nodes, *body) || handler.is_some_and(|handler| graph_binds_slots(nodes, handler))
        }
        ProgramNode::ChainBody { body } | ProgramNode::Label { body, .. } => graph_binds_slots(nodes, *body),
        ProgramNode::ConstructObject { members, .. } => members
            .iter()
            .any(|member| graph_binds_slots(nodes, member.key) || graph_binds_slots(nodes, member.value)),
        ProgramNode::Call { args, .. } => args.iter().any(|arg| graph_binds_slots(nodes, *arg)),
        ProgramNode::EngineGenerator { init, update, extract } => {
            graph_binds_slots(nodes, *init) || graph_binds_slots(nodes, *update) || graph_binds_slots(nodes, *extract)
        }
        ProgramNode::EngineRng { seed } => graph_binds_slots(nodes, *seed),
        ProgramNode::Counted { source, .. } => graph_binds_slots(nodes, *source),
        ProgramNode::Modify { paths, update, .. } | ProgramNode::FactAssign { paths, update, .. } => {
            graph_binds_slots(nodes, *paths) || graph_binds_slots(nodes, *update)
        }
    }
}

/// A single-output binder-free argument graph: eligible to evaluate inline
/// on the caller's machine (shared env, isolated frames) instead of seeding
/// a nested one.
pub(crate) fn argument_inline_eligible(nodes: &[ProgramNode], node: ProgramNodeId) -> bool {
    graph_at_most_one(nodes, node) && !graph_binds_slots(nodes, node)
}

/// Whether `node`'s graph reads any binder slot. A frozen argument that
/// neither binds nor reads can skip the overlay snapshot.
pub(crate) fn graph_reads_any_slot(nodes: &[ProgramNode], node: ProgramNodeId) -> bool {
    graph_reads_any_slot_walk(nodes, node, &mut Vec::new())
}

#[expect(
    clippy::too_many_lines,
    reason = "one exhaustive arm per program node: the walk IS the read-slot inventory, and splitting it would hide which variants are covered"
)]
fn graph_reads_any_slot_walk(nodes: &[ProgramNode], node: ProgramNodeId, visiting: &mut Vec<ProgramNodeId>) -> bool {
    match &nodes[node.index()] {
        ProgramNode::Stage { start, steps } => {
            if matches!(start, StageStart::Variable(_)) {
                return true;
            }
            steps.iter().any(|step| match step.access() {
                StepAccess::DynVar(_) | StepAccess::DynNodeAccessor(_) | StepAccess::DynAttribute(_) => true,
                StepAccess::Slice(bounds) => {
                    matches!(bounds.start, crate::program::SliceBound::Var(_))
                        || matches!(bounds.end, crate::program::SliceBound::Var(_))
                }
                _ => false,
            })
        }
        ProgramNode::Counted { .. } => true,
        ProgramNode::Empty
        | ProgramNode::CallFilter { .. }
        | ProgramNode::EnginePull { .. }
        | ProgramNode::Break { .. } => false,
        ProgramNode::CallDef {
            body,
            param_slots,
            args,
            filter_args,
            ..
        } => {
            if !param_slots.is_empty() {
                return true;
            }
            if args.iter().any(|arg| graph_reads_any_slot_walk(nodes, *arg, visiting)) {
                return true;
            }
            if filter_args
                .iter()
                .any(|arg| graph_reads_any_slot_walk(nodes, *arg, visiting))
            {
                return true;
            }
            if visiting.contains(body) {
                return false;
            }
            visiting.push(*body);
            let reads = graph_reads_any_slot_walk(nodes, *body, visiting);
            visiting.pop();
            reads
        }
        ProgramNode::Binary { left, right, .. }
        | ProgramNode::FlatMap {
            upstream: left,
            body: right,
        }
        | ProgramNode::Choice { left, right }
        | ProgramNode::Alternative { left, right }
        | ProgramNode::Logical { left, right, .. }
        | ProgramNode::Bind {
            source: left,
            body: right,
            ..
        }
        | ProgramNode::EngineBind {
            source: left,
            body: right,
            ..
        } => graph_reads_any_slot_walk(nodes, *left, visiting) || graph_reads_any_slot_walk(nodes, *right, visiting),
        ProgramNode::CollectArray { body } | ProgramNode::CountCollect { body } => {
            body.is_some_and(|body| graph_reads_any_slot_walk(nodes, body, visiting))
        }
        ProgramNode::Concat { parts } => parts
            .iter()
            .any(|part| graph_reads_any_slot_walk(nodes, *part, visiting)),
        ProgramNode::Conditional {
            condition,
            consequent,
            alternative,
        } => {
            graph_reads_any_slot_walk(nodes, *condition, visiting)
                || graph_reads_any_slot_walk(nodes, *consequent, visiting)
                || graph_reads_any_slot_walk(nodes, *alternative, visiting)
        }
        ProgramNode::Try { body, handler } => {
            graph_reads_any_slot_walk(nodes, *body, visiting)
                || handler.is_some_and(|handler| graph_reads_any_slot_walk(nodes, handler, visiting))
        }
        ProgramNode::ChainBody { body } | ProgramNode::Label { body, .. } => {
            graph_reads_any_slot_walk(nodes, *body, visiting)
        }
        ProgramNode::ConstructObject { members, .. } => members.iter().any(|member| {
            graph_reads_any_slot_walk(nodes, member.key, visiting)
                || graph_reads_any_slot_walk(nodes, member.value, visiting)
        }),
        ProgramNode::Call { args, .. } => args.iter().any(|arg| graph_reads_any_slot_walk(nodes, *arg, visiting)),
        ProgramNode::EngineGenerator { init, update, extract } => {
            graph_reads_any_slot_walk(nodes, *init, visiting)
                || graph_reads_any_slot_walk(nodes, *update, visiting)
                || graph_reads_any_slot_walk(nodes, *extract, visiting)
        }
        ProgramNode::EngineRng { seed } => graph_reads_any_slot_walk(nodes, *seed, visiting),
        ProgramNode::Reduce {
            source,
            init,
            update,
            keyed_collect,
            ..
        } => {
            graph_reads_any_slot_walk(nodes, *source, visiting)
                || graph_reads_any_slot_walk(nodes, *init, visiting)
                || graph_reads_any_slot_walk(nodes, *update, visiting)
                || keyed_collect.is_some_and(|node| graph_reads_any_slot_walk(nodes, node, visiting))
        }
        ProgramNode::Foreach {
            source,
            init,
            update,
            extract,
            ..
        } => {
            graph_reads_any_slot_walk(nodes, *source, visiting)
                || graph_reads_any_slot_walk(nodes, *init, visiting)
                || graph_reads_any_slot_walk(nodes, *update, visiting)
                || extract.is_some_and(|node| graph_reads_any_slot_walk(nodes, node, visiting))
        }
        ProgramNode::Modify { paths, update, .. } | ProgramNode::FactAssign { paths, update, .. } => {
            graph_reads_any_slot_walk(nodes, *paths, visiting) || graph_reads_any_slot_walk(nodes, *update, visiting)
        }
    }
}

/// Slot indices `node`'s graph reads, including recursive `CallDef` bodies
/// (guarded) and the callee's param slots.
pub(crate) fn collect_read_slots(nodes: &[ProgramNode], node: ProgramNodeId, out: &mut Vec<VarSlot>) {
    collect_read_slots_walk(nodes, node, out, &mut Vec::new());
}

#[expect(
    clippy::too_many_lines,
    reason = "one exhaustive arm per program node: the walk IS the read-slot inventory, and splitting it would hide which variants are covered"
)]
fn collect_read_slots_walk(
    nodes: &[ProgramNode],
    node: ProgramNodeId,
    out: &mut Vec<VarSlot>,
    visiting: &mut Vec<ProgramNodeId>,
) {
    match &nodes[node.index()] {
        ProgramNode::Stage { start, steps } => {
            if let StageStart::Variable(slot) = start {
                out.push(*slot);
            }
            for step in steps {
                match step.access() {
                    StepAccess::DynVar(slot) | StepAccess::DynNodeAccessor(slot) | StepAccess::DynAttribute(slot) => {
                        out.push(*slot);
                    }
                    StepAccess::Slice(bounds) => {
                        if let crate::program::SliceBound::Var(slot) = bounds.start {
                            out.push(slot);
                        }
                        if let crate::program::SliceBound::Var(slot) = bounds.end {
                            out.push(slot);
                        }
                    }
                    _ => {}
                }
            }
        }
        ProgramNode::Counted { count, source, .. } => {
            out.push(*count);
            collect_read_slots_walk(nodes, *source, out, visiting);
        }
        ProgramNode::Empty
        | ProgramNode::CallFilter { .. }
        | ProgramNode::EnginePull { .. }
        | ProgramNode::Break { .. } => {}
        ProgramNode::CallDef {
            body,
            param_slots,
            args,
            filter_args,
            ..
        } => {
            out.extend(param_slots.iter().copied());
            for arg in args {
                collect_read_slots_walk(nodes, *arg, out, visiting);
            }
            for arg in filter_args {
                collect_read_slots_walk(nodes, *arg, out, visiting);
            }
            if !visiting.contains(body) {
                visiting.push(*body);
                collect_read_slots_walk(nodes, *body, out, visiting);
                visiting.pop();
            }
        }
        ProgramNode::Binary { left, right, .. }
        | ProgramNode::FlatMap {
            upstream: left,
            body: right,
        }
        | ProgramNode::Choice { left, right }
        | ProgramNode::Alternative { left, right }
        | ProgramNode::Logical { left, right, .. }
        | ProgramNode::Bind {
            source: left,
            body: right,
            ..
        }
        | ProgramNode::EngineBind {
            source: left,
            body: right,
            ..
        } => {
            collect_read_slots_walk(nodes, *left, out, visiting);
            collect_read_slots_walk(nodes, *right, out, visiting);
        }
        ProgramNode::CollectArray { body } | ProgramNode::CountCollect { body } => {
            if let Some(body) = body {
                collect_read_slots_walk(nodes, *body, out, visiting);
            }
        }
        ProgramNode::Concat { parts } => {
            for part in parts {
                collect_read_slots_walk(nodes, *part, out, visiting);
            }
        }
        ProgramNode::Conditional {
            condition,
            consequent,
            alternative,
        } => {
            collect_read_slots_walk(nodes, *condition, out, visiting);
            collect_read_slots_walk(nodes, *consequent, out, visiting);
            collect_read_slots_walk(nodes, *alternative, out, visiting);
        }
        ProgramNode::Try { body, handler } => {
            collect_read_slots_walk(nodes, *body, out, visiting);
            if let Some(handler) = handler {
                collect_read_slots_walk(nodes, *handler, out, visiting);
            }
        }
        ProgramNode::ChainBody { body } | ProgramNode::Label { body, .. } => {
            collect_read_slots_walk(nodes, *body, out, visiting);
        }
        ProgramNode::ConstructObject { members, .. } => {
            for member in members {
                collect_read_slots_walk(nodes, member.key, out, visiting);
                collect_read_slots_walk(nodes, member.value, out, visiting);
            }
        }
        ProgramNode::Call { args, .. } => {
            for arg in args {
                collect_read_slots_walk(nodes, *arg, out, visiting);
            }
        }
        ProgramNode::EngineGenerator { init, update, extract } => {
            collect_read_slots_walk(nodes, *init, out, visiting);
            collect_read_slots_walk(nodes, *update, out, visiting);
            collect_read_slots_walk(nodes, *extract, out, visiting);
        }
        ProgramNode::EngineRng { seed } => {
            collect_read_slots_walk(nodes, *seed, out, visiting);
        }
        ProgramNode::Reduce {
            source,
            init,
            update,
            keyed_collect,
            ..
        } => {
            collect_read_slots_walk(nodes, *source, out, visiting);
            collect_read_slots_walk(nodes, *init, out, visiting);
            collect_read_slots_walk(nodes, *update, out, visiting);
            if let Some(node) = keyed_collect {
                collect_read_slots_walk(nodes, *node, out, visiting);
            }
        }
        ProgramNode::Foreach {
            source,
            init,
            update,
            extract,
            ..
        } => {
            collect_read_slots_walk(nodes, *source, out, visiting);
            collect_read_slots_walk(nodes, *init, out, visiting);
            collect_read_slots_walk(nodes, *update, out, visiting);
            if let Some(node) = extract {
                collect_read_slots_walk(nodes, *node, out, visiting);
            }
        }
        ProgramNode::Modify { paths, update, .. } | ProgramNode::FactAssign { paths, update, .. } => {
            collect_read_slots_walk(nodes, *paths, out, visiting);
            collect_read_slots_walk(nodes, *update, out, visiting);
        }
    }
}

/// Fills [`BinaryShape`] on every `Binary` after fuse. Identity × literal/slot
/// is the frame-free pair; everything else stays `Framed`.
pub(crate) fn mark_binary_shapes(nodes: &mut [ProgramNode]) {
    let shapes: Vec<Option<crate::program::BinaryShape>> = nodes
        .iter()
        .map(|node| match node {
            ProgramNode::Binary { left, right, .. } => {
                if !identity_stage(nodes, *left) {
                    Some(crate::program::BinaryShape::Framed)
                } else if constant_operand(nodes, *right).is_some() {
                    Some(crate::program::BinaryShape::IdentityLiteral)
                } else if matches!(
                    &nodes[right.index()],
                    ProgramNode::Stage {
                        start: StageStart::Variable(_),
                        steps,
                    } if steps.is_empty()
                ) {
                    let ProgramNode::Stage {
                        start: StageStart::Variable(slot),
                        ..
                    } = &nodes[right.index()]
                    else {
                        unreachable!("just matched");
                    };
                    Some(crate::program::BinaryShape::IdentitySlot(*slot))
                } else {
                    Some(crate::program::BinaryShape::Framed)
                }
            }
            _ => None,
        })
        .collect();
    for (node, shape) in nodes.iter_mut().zip(shapes) {
        if let (ProgramNode::Binary { shape: slot, .. }, Some(shape)) = (node, shape) {
            *slot = shape;
        }
    }
}

pub(crate) fn identity_stage(nodes: &[ProgramNode], node: ProgramNodeId) -> bool {
    matches!(
        &nodes[node.index()],
        ProgramNode::Stage {
            start: StageStart::Current,
            steps,
        } if steps.is_empty()
    )
}

pub(crate) fn take_or_borrow<'src>(
    retained: &mut EngineResult<'src>,
    spent: bool,
) -> Result<EngineResult<'src>, EngineRunError> {
    if spent {
        return Ok(core::mem::replace(retained, EngineResult::owned(Value::Null)));
    }
    retained.try_clone().map_err(EngineRunError::Codec)
}
