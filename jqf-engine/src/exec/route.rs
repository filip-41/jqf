//! Emission routing and the break-unwind continuation seam.
//!
//! One job: walk a produced value through `frames[0..cont]` to the first
//! consumer, and keep `cont` inside the live stack after a `limit`/`nth`
//! cut or a `label`/`break` unwind. Collect/object construction and
//! `advance_frame` sit here because they are the routing walk's partners.

#[allow(clippy::wildcard_imports)]
use super::*;

/// What the first object member seeds with: a static key already materialized
/// (its value producer evaluates next) or the node of a dynamic key producer.
enum FirstMemberSeed {
    Static(Value),
    Dynamic(ProgramNodeId),
}

impl<'program, 'source> GraphMachine<'program, 'source> {
    /// Routes a produced value through `frames[0..cont]` (top-down) to the first
    /// CONSUMER frame — the emission-routing law generalized from "first
    /// `FlatMapBody` consumes" to "first CONSUMER frame consumes". A `FlatMapBody`
    /// evaluates its body over the value; a `Collect` captures it into its array;
    /// an `ObjectKey`/`ObjectValue` advances object construction; an `ObjectDest`,
    /// `Each`, or `Choice` is a producer/anchor the walk skips. If no consumer is
    /// in range, the value is an ordered output item (`Some`).
    ///
    /// `cont` is reconciled against the live stack: a `limit`/`nth` cut (or a
    /// `label`/`break`) may have dropped frames after a consumer captured this
    /// depth. Walking past `frames.len()` is an uncatchable panic; clamping to
    /// the frames that remain is the continuation-safe model `first`/`last`
    /// already enjoy, applied at this one seam so every consumer position is
    /// covered.
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per consumer frame: the routing walk IS the table, and splitting it \
                  would hide which consumers are covered"
    )]
    pub(crate) fn route(
        &mut self,
        value: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        // A barrier (Try, Label) whose body has completed is spent: every frame its
        // evaluation pushed is gone, and `advance_frame` would pop it silently.
        // But when the body's output routes through a consumer (LogicalLeft,
        // BinaryLeft, object-construction frames), that consumer pushes NEW frames
        // ABOVE the barrier, burying it before `advance_frame` can reach it. A
        // later raise from a sibling branch would then find the stale barrier and
        // admit it on the positional guard (its `out_cont <= raise_cont` when the
        // sibling's continuation includes the barrier's index). Pop spent frames
        // at-or-above `cont` now, BEFORE any consumer pushes on top — the guard
        // `out_cont <= raise_cont` preserves the real downstream-error escape
        // (`try .a | .b` and `(.a)?.b` still error at `.b`).
        self.trim_spent_frames(cont, resources);
        let cont = cont.min(self.frames.len());
        for index in (0..cont).rev() {
            match &self.frames.as_slice()[index] {
                // The path-family body's emission consumer: a tracked output
                // publishes the register's path, anything else raises.
                GraphFrame::PathPublish { .. } => {
                    return self
                        .consume_path_publish(index, &value, resources)
                        .map(|()| None);
                }
                GraphFrame::FlatMapBody { body, out_cont } => {
                    let (body, out_cont) = (*body, *out_cont);
                    self.pending = Some(GraphTask::Eval {
                        node: body,
                        input: value,
                        cont: out_cont,
                    });
                    return Ok(None);
                }
                GraphFrame::Collect { .. } => {
                    return self.consume_collect(index, value, resources).map(|()| None);
                }
                GraphFrame::CountCollect { .. } => {
                    return self.consume_count_collect(index);
                }
                GraphFrame::ObjectKey { dest, member } => {
                    let (dest, member) = (*dest, *member);
                    return self
                        .consume_object_key(dest, member, &value)
                        .map(|()| None);
                }
                GraphFrame::ObjectValue { dest, member, key } => {
                    let (dest, member) = (*dest, *member);
                    // Every value this key produces reuses the SAME key
                    // emission: a Cartesian product over the members repeats
                    // the key once per combination, and a shared handle makes
                    // that repeat a refcount bump instead of a fresh capture.
                    let key = key.try_clone().map_err(EngineRunError::Codec)?;
                    return self.consume_object_value(dest, member, &key, value, resources);
                }
                GraphFrame::SelectBody { out_cont, .. } => {
                    let out_cont = *out_cont;
                    return self.consume_select(index, out_cont, &value, resources);
                }
                // A countdown is a CONSUMER: every source item stops here, and
                // the frame decides whether it continues downstream at all.
                GraphFrame::Counted { .. } => {
                    return self.consume_counted(index, value, resources);
                }
                GraphFrame::BinaryRight { op, left, out_cont, .. } => {
                    let (op, left, out_cont) = (*op, *left, *out_cont);
                    return self.consume_binary_right(index, op, left, out_cont, value, resources);
                }
                GraphFrame::BinaryLeft { op, out_cont, .. } => {
                    let (op, out_cont) = (*op, *out_cont);
                    return self.consume_binary_left(index, op, out_cont, value, resources);
                }
                GraphFrame::ConcatPart { .. } => {
                    return self.consume_concat(index, value, resources);
                }
                // The math argument nesting consumes each outer output (one
                // nested drive per value) and the innermost phase computes and
                // publishes one result per first-argument output.
                GraphFrame::MathArgs { .. } => {
                    return self.consume_math_args(index, value, resources);
                }
                GraphFrame::MathCompute { .. } => {
                    return self.consume_math_compute(index, value, resources);
                }
                GraphFrame::ConditionalBody {
                    consequent,
                    alternative,
                    out_cont,
                    ..
                } => {
                    let (consequent, alternative, out_cont) =
                        (*consequent, *alternative, *out_cont);
                    return self.consume_conditional(
                        index,
                        consequent,
                        alternative,
                        out_cont,
                        &value,
                    );
                }
                GraphFrame::AlternativeLeft { out_cont, .. } => {
                    let out_cont = *out_cont;
                    return self.consume_alternative_left(index, out_cont, value, resources);
                }
                GraphFrame::LogicalLeft {
                    operator,
                    right,
                    out_cont,
                    ..
                } => {
                    let (operator, right, out_cont) = (*operator, *right, *out_cont);
                    return self.consume_logical_left(
                        index,
                        operator,
                        right,
                        out_cont,
                        &value,
                        resources,
                    );
                }
                GraphFrame::LogicalRight { out_cont } => {
                    let out_cont = *out_cont;
                    return self.consume_logical_right(out_cont, &value, resources);
                }
                GraphFrame::ErrorArg { out_cont } => {
                    // `error/1`: raise the argument's FIRST output as the error,
                    // scoped to the call's routing position.
                    self.raise_cont = *out_cont;
                    let raised = self.materialize_capture(value, resources)?;
                    return Err(EngineRunError::Raised(raised));
                }
                // The binder family's four consumer flavors. `BindBody` and the
                // two loop-source frames rebind the slot; the two init frames
                // start a run; `ReduceUpdate` consumes silently (the fold has no
                // mid-run output) while `ForeachUpdate` is the CONSUMER THAT
                // RE-EMITS — it consumes the update output and publishes it in the
                // same transition.
                GraphFrame::BindBody { .. } => {
                    return self.consume_bind_body(index, value, resources);
                }
                GraphFrame::ReduceInit { .. } | GraphFrame::ForeachInit { .. } => {
                    return self.consume_loop_init(index, value, resources);
                }
                GraphFrame::ReduceSource { .. } | GraphFrame::ForeachSource { .. } => {
                    return self.consume_loop_source(index, value);
                }
                GraphFrame::ReduceUpdate { .. } => {
                    return self.consume_reduce_update(index, value, resources);
                }
                GraphFrame::ForeachUpdate { .. } => {
                    return self.consume_foreach_update(index, value, resources);
                }
                // The `~generator` phase drive's consumer: init/update outputs
                // become the one state, extract outputs are the pull's emitted
                // value, re-routed through the cursor machine's own ceiling.
                GraphFrame::EngineGen { .. } => {
                    return self.consume_engine_gen(index, value, resources);
                }
                // The `~rng` seed consumer: the seed's one integer output arms
                // the generator; the Emit phase never consumes (each draw
                // routes directly from the advance arm).
                GraphFrame::EngineRng { .. } => {
                    return self.consume_engine_rng(index, value, resources);
                }
                // The keyed-collect drive's two consumers: the SOURCE frame
                // binds each source item and starts its key graph; the KEY
                // frame inserts each key into the builder and stays for the
                // next one (nothing publishes until the source is spent).
                GraphFrame::KeyedCollectSource { .. } => {
                    return self.consume_keyed_source(index, value);
                }
                GraphFrame::KeyedCollectKey { .. } => {
                    return self.consume_keyed_key(index, value, resources);
                }
                // The streaming aggregate drive's two consumers: the SOURCE
                // frame holds each generated element pending and starts its key
                // filter; the KEY frame captures each key output (filing into
                // the accumulator happens when the filter exhausts).
                GraphFrame::KeyedStreamSource { .. } => {
                    return self
                        .consume_keyed_stream_source(index, value, resources)
                        .map(|()| None);
                }
                GraphFrame::KeyedStreamKey { .. } => {
                    return self.consume_keyed_stream_key(index, value, resources).map(|()| None);
                }
                // The assignment fold's UPDATE consumer: it consumes silently
                // (the fold publishes once, at the end) and keeps only the FIRST
                // output.
                GraphFrame::ModifyUpdate { .. } => {
                    return self.consume_modify_update(index, value, resources);
                }
                // The fact-assignment UPDATE consumer: the FIRST output becomes
                // the node's payload (recorded as a delta); the document routes
                // when the update completes.
                GraphFrame::FactAssignWrite { .. } => {
                    return self.consume_fact_assign(index, value, resources);
                }
                // The keyed family's key consumer: every output of one child's
                // key filter is captured into that child's key array, and
                // nothing is published until the whole collection closes.
                GraphFrame::KeyedCollect { .. } => {
                    return self.consume_keyed(index, value, resources).map(|()| None);
                }
                // The answering argument consumer: the CONSUMER THAT RE-EMITS —
                // each argument output is answered and published in the same
                // transition.
                GraphFrame::ArgumentAnswer { .. } => {
                    return self.consume_answer(index, value, resources);
                }
                // The `setpath` drive's two consumer phases: a VALUE output
                // opens the PATH argument's inner drive, a PATH output answers
                // the pair and publishes the rewritten document.
                GraphFrame::PathSetValue { .. } => {
                    return self.consume_path_set_value(index, value, resources);
                }
                GraphFrame::PathSetPair { .. } => {
                    return self.consume_path_set_pair(index, value, resources);
                }
                // `walk`'s child consumer: an array arm collects silently, an
                // object arm takes one output and cuts. Neither publishes here —
                // the container publishes when its last child is done.
                GraphFrame::Walk { .. } => {
                    return self.consume_walk(index, value, resources);
                }
                // `map_values`'s child consumer: BOTH arms take the first
                // output and cut — the `.[] |= f` cap, single-level.
                GraphFrame::MapValues { .. } => {
                    return self.consume_map_values(index, value, resources);
                }
                // The generator family's one frame is BOTH classes, decided by
                // its state: a bound/argument consumer stops the walk, while a
                // mid-stream source is a producer and is skipped exactly as
                // `Each` is.
                GraphFrame::Generate { state } if state.consumes() => {
                    return self.consume_generate(index, value, resources);
                }
                // A `Try` barrier is transparent to emissions (the fourth routing
                // behavior): the walk skips it, so body outputs reach the same
                // consumers the `try`'s own outputs would. A `Label` is the same
                // barrier family and is skipped identically — `label $o | 1, 2`
                // emits `1 2` exactly as the unlabelled body would.
                GraphFrame::Generate { .. }
                | GraphFrame::Each { .. }
                // An `IndexedEach` is `Each`'s producer twin and is skipped
                // identically: it changes which children are produced, never
                // where a produced child is routed.
                | GraphFrame::IndexedEach { .. }
                | GraphFrame::Choice { .. }
                | GraphFrame::ObjectDest { .. }
                // A `PathEmit` is a producer of already-computed values: it
                // changes WHICH values are published, never where a published
                // one is routed. A `ModifyFold` is the same class: it publishes
                // its ONE document from `advance_frame`, never from this walk.
                | GraphFrame::PathEmit { .. }
                // A `PathStep` marker is TRANSPARENT to emissions: it scopes
                // the register extension, never routing, so the walk skips it.
                | GraphFrame::PathStep { .. }
                // An `InputsEmit` is the same producer class, pulling from the
                // shared input source in `advance_frame`.
                | GraphFrame::InputsEmit { .. }
                | GraphFrame::SelectorEmit { .. }
                // A `PathWalkEmit` is the same producer class: it publishes its
                // walked paths from `advance_frame`, never from this walk.
                // A `TostreamEmit` publishes its walked pair/marker stream from
                // `advance_frame` — the same class again.
                | GraphFrame::PathWalkEmit { .. }
                | GraphFrame::TostreamEmit { .. }
                | GraphFrame::ModifyFold { .. }
                // A `CallableBody` is a producer in the same class: it changes
                // WHICH values are published, never where a published one is
                // routed, so the walk skips it exactly as it skips `PathEmit`.
                // An `EngineBindBody` is a TRANSPARENT scope barrier (the
                // body's outputs route past it to the binding's continuation)
                // and an `EnginePull` publishes from `advance_frame` — both
                // are producer-class and skipped by the walk exactly as the
                // frames above are.
                | GraphFrame::CallableBody { .. }
                | GraphFrame::CallFilter { .. }
                | GraphFrame::EngineBindBody { .. }
                | GraphFrame::EnginePull { .. }
                | GraphFrame::Try { .. }
                | GraphFrame::Label { .. } => {}
            }
        }
        Ok(Some(value))
    }

    /// Evaluates a `CollectArray` over `input`: an absent body is the empty array
    /// `[]` routed directly; a present body pushes a `Collect` destination frame
    /// and drives the body into it.
    pub(crate) fn eval_collect(
        &mut self,
        body: Option<ProgramNodeId>,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let Some(body) = body else {
            let array = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
            return self.route(EngineResult::owned(Value::Array(array)), cont, resources);
        };
        // The collect body is a frozen subexpression.
        self.path_enter_frozen();
        let array = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
        self.push_frame(GraphFrame::Collect { array, out_cont: cont });
        self.pending = Some(GraphTask::Eval {
            node: body,
            input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Evaluates a `CountCollect` over `input`: `[f] | length` with the array
    /// never materialized. The body
    /// drives into a counting frame exactly as it would into a `Collect` frame
    /// — same error propagation, same empty-body law — and the finish routes
    /// the exact integer count. Semantically identical to the collect-then-
    /// length spelling: `length` of the constructed array IS its element count.
    pub(crate) fn eval_count_collect(
        &mut self,
        body: Option<ProgramNodeId>,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let Some(body) = body else {
            let value = Value::Number(Number::integer(jqf_data::Integer::from_i64(0)));
            return self.route(EngineResult::owned(value), cont, resources);
        };
        self.path_enter_frozen();
        self.push_frame(GraphFrame::CountCollect {
            count: 0,
            out_cont: cont,
        });
        self.pending = Some(GraphTask::Eval {
            node: body,
            input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Increments the count-collect consumer the emission walk already named.
    #[allow(
        clippy::unnecessary_wraps,
        reason = "same Result<Option<EngineResult>> shape as every other emission consumer"
    )]
    fn consume_count_collect(&mut self, index: usize) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let GraphFrame::CountCollect { count, .. } = self.routed_frame_mut(index) else {
            unreachable!("emission walk already classified CountCollect");
        };
        *count = count.saturating_add(1);
        Ok(None)
    }

    /// Consumes one body output into a `Collect` frame's array: materialize it at
    /// capture (the barrier), then push it.
    pub(crate) fn consume_collect(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        let owned = self.materialize_capture(value, resources)?;
        let GraphFrame::Collect { array, .. } = &mut self.frames.as_mut_slice()[index] else {
            return Err(internal_contract("collect frame kind changed under capture"));
        };
        array.try_push(owned).map_err(|_| EngineRunError::allocation_failure())
    }

    /// Evaluates a `ConstructObject` over `input`: an empty member list is the
    /// empty object `{}` routed directly; otherwise it pushes an `ObjectDest`
    /// anchor holding the original input and starts the first member.
    pub(crate) fn eval_construct_object(
        &mut self,
        ctor: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        if self.object_members(ctor)?.is_empty() {
            let object = ObjectBuilder::new()
                .try_finish()
                .map_err(|_| EngineRunError::allocation_failure())?;
            return self.route(EngineResult::owned(Value::Object(object)), cont, resources);
        }
        // EVERY fallible step of the first member's seeding runs BEFORE the
        // anchor exists. A refusal after `path_enter_frozen` would leak the
        // freeze (no destination frame will ever pop to release it), and a
        // destination pushed without its consumer frame would be popped by
        // the exhausted-frame arm as if its members were done — resuming the
        // parent with this construction having routed nothing.
        let members = self.object_members(ctor)?;
        let single_combination = members.iter().all(|member| {
            crate::exec::graph_at_most_one(self.nodes, member.key)
                && crate::exec::graph_at_most_one(self.nodes, member.value)
        });
        let first_seed = match members[0].static_key.as_deref() {
            Some(text) => {
                FirstMemberSeed::Static(Value::try_string(text).map_err(|_| EngineRunError::allocation_failure())?)
            }
            None => FirstMemberSeed::Dynamic(members[0].key),
        };
        let seed_input = input.try_clone().map_err(EngineRunError::Codec)?;
        // The members' key/value subexpressions run frozen (the scope ends
        // when the destination frame pops).
        self.path_enter_frozen();
        self.push_frame(GraphFrame::ObjectDest {
            input,
            ctor,
            partial: Vec::new(),
            out_cont: cont,
            single_combination,
        });
        let dest = self.frames.len() - 1;
        match first_seed {
            FirstMemberSeed::Static(key) => {
                let value_node = members[0].value;
                self.push_frame(GraphFrame::ObjectValue {
                    dest,
                    member: 0,
                    key: EngineResult::owned(key),
                });
                self.pending = Some(GraphTask::Eval {
                    node: value_node,
                    input: seed_input,
                    cont: self.frames.len(),
                });
            }
            FirstMemberSeed::Dynamic(key_node) => {
                self.push_frame(GraphFrame::ObjectKey { dest, member: 0 });
                self.pending = Some(GraphTask::Eval {
                    node: key_node,
                    input: seed_input,
                    cont: self.frames.len(),
                });
            }
        }
        Ok(None)
    }

    /// Starts one object member's key iteration: pushes an `ObjectKey` consumer and
    /// evaluates the member's key producer over the constructor's ORIGINAL input.
    pub(crate) fn start_object_member(
        &mut self,
        dest: usize,
        member: usize,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let ctor = self.object_dest_ctor(dest)?;
        let members = self.object_members(ctor)?;
        let input = self.object_dest_input(dest)?;
        if let Some(text) = members[member].static_key.as_deref() {
            let value_node = members[member].value;
            let key = EngineResult::owned(Value::try_string(text).map_err(|_| EngineRunError::allocation_failure())?);
            self.push_frame(GraphFrame::ObjectValue { dest, member, key });
            self.pending = Some(GraphTask::Eval {
                node: value_node,
                input,
                cont: self.frames.len(),
            });
            return Ok(None);
        }
        let key_node = members[member].key;
        self.push_frame(GraphFrame::ObjectKey { dest, member });
        self.pending = Some(GraphTask::Eval {
            node: key_node,
            input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Consumes one key output for an object member: validates it as a core string
    /// (a non-string or tagged string is the object-key mismatch class), then
    /// starts that key's value iteration.
    pub(crate) fn consume_object_key(
        &mut self,
        dest: usize,
        member: usize,
        value: &EngineResult<'source>,
    ) -> Result<(), EngineRunError> {
        // The key is NOT validated here: the key producer runs first (its own
        // raises propagate) but the key is checked to be a string only once a
        // value has been produced. The raw emission rides in the
        // `ObjectValue` frame and is validated per value output.
        let key = value.try_clone().map_err(EngineRunError::Codec)?;
        let ctor = self.object_dest_ctor(dest)?;
        let value_node = self.object_members(ctor)?[member].value;
        let input = self.object_dest_input(dest)?;
        self.push_frame(GraphFrame::ObjectValue { dest, member, key });
        self.pending = Some(GraphTask::Eval {
            node: value_node,
            input,
            cont: self.frames.len(),
        });
        Ok(())
    }

    /// Consumes one value output for an object member: materialize it at capture,
    /// record `(key, value)` in the destination's partial, then either start the
    /// next member or (at the last member) finish one object combination and route
    /// it through the destination's out-continuation.
    pub(crate) fn consume_object_value(
        &mut self,
        dest: usize,
        member: usize,
        key: &EngineResult<'source>,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        // A non-string key fails the object routing at the destination's out_cont.
        // The check is HERE, per value output, because a key is validated only
        // when its value produces: an empty member's key is never checked
        // (`{(.): empty}` answers nothing) and a value that raises reports its
        // own error first.
        let key = extract_key_string(key, resources).inspect_err(|_| {
            if let Ok(out_cont) = self.object_dest_out_cont(dest) {
                self.raise_cont = out_cont;
            }
        })?;
        let owned = self.materialize_capture(value, resources)?;
        self.truncate_partial(dest, member)?;
        self.push_partial(dest, key, owned)?;
        let ctor = self.object_dest_ctor(dest)?;
        let member_count = self.object_members(ctor)?.len();
        if member + 1 < member_count {
            self.start_object_member(dest, member + 1)
        } else {
            let object = self.build_object(dest, resources)?;
            let out_cont = self.object_dest_out_cont(dest)?;
            self.route(EngineResult::owned(object), out_cont, resources)
        }
    }

    /// The members of a `ConstructObject` node.
    pub(crate) fn object_members(&self, ctor: ProgramNodeId) -> Result<&'program [ObjectMemberNode], EngineRunError> {
        match &self.nodes[ctor.index()] {
            ProgramNode::ConstructObject { members } => Ok(members),
            _ => Err(internal_contract("object destination over a non-object node")),
        }
    }

    /// The `ctor` node id held by an `ObjectDest` frame.
    pub(crate) fn object_dest_ctor(&self, dest: usize) -> Result<ProgramNodeId, EngineRunError> {
        match self.frames.as_slice().get(dest) {
            Some(GraphFrame::ObjectDest { ctor, .. }) => Ok(*ctor),
            _ => Err(internal_contract("object destination frame kind changed")),
        }
    }

    /// The out-continuation held by an `ObjectDest` frame.
    pub(crate) fn object_dest_out_cont(&self, dest: usize) -> Result<usize, EngineRunError> {
        match self.frames.as_slice().get(dest) {
            Some(GraphFrame::ObjectDest { out_cont, .. }) => Ok(*out_cont),
            _ => Err(internal_contract("object destination frame kind changed")),
        }
    }

    /// A fresh re-borrow of an `ObjectDest` frame's original input (fallible at the
    /// fork, Arc-cheap for located values).
    pub(crate) fn object_dest_input(&self, dest: usize) -> Result<EngineResult<'source>, EngineRunError> {
        match self.frames.as_slice().get(dest) {
            Some(GraphFrame::ObjectDest { input, .. }) => input.try_clone().map_err(EngineRunError::Codec),
            _ => Err(internal_contract("object destination frame kind changed")),
        }
    }

    /// Truncates an `ObjectDest` frame's partial captures back to `len` (the depth
    /// of the enclosing members) when backtracking.
    pub(crate) fn truncate_partial(&mut self, dest: usize, len: usize) -> Result<(), EngineRunError> {
        match self.frames.as_mut_slice().get_mut(dest) {
            Some(GraphFrame::ObjectDest { partial, .. }) => {
                partial.truncate(len);
                Ok(())
            }
            _ => Err(internal_contract("object destination frame kind changed")),
        }
    }

    /// Records one captured `(key, value)` pair in an `ObjectDest` frame's partial.
    pub(crate) fn push_partial(
        &mut self,
        dest: usize,
        key: jqf_data::ObjectKey,
        value: Value,
    ) -> Result<(), EngineRunError> {
        match self.frames.as_mut_slice().get_mut(dest) {
            Some(GraphFrame::ObjectDest { partial, .. }) => {
                partial
                    .try_reserve(1)
                    .map_err(|_| EngineRunError::allocation_failure())?;
                partial.push((key, value));
                Ok(())
            }
            _ => Err(internal_contract("object destination frame kind changed")),
        }
    }

    /// Finishes one object combination from an `ObjectDest` frame's partial through
    /// `jqf_data::ObjectBuilder` (first-occurrence position, last-occurrence value).
    /// A proven single combination moves the partial; otherwise the pairs are
    /// cloned so sibling Cartesian combinations still see them.
    pub(crate) fn build_object(
        &mut self,
        dest: usize,
        _resources: &ResourceContext<'_>,
    ) -> Result<Value, EngineRunError> {
        let Some(GraphFrame::ObjectDest {
            partial,
            single_combination,
            ..
        }) = self.frames.as_mut_slice().get_mut(dest)
        else {
            return Err(internal_contract("object destination frame kind changed"));
        };
        let mut builder =
            ObjectBuilder::try_with_capacity(partial.len()).map_err(|_| EngineRunError::allocation_failure())?;
        if *single_combination {
            for (key, value) in core::mem::take(partial) {
                builder
                    .try_insert_last(key, value)
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
        } else {
            for (key, value) in partial.iter() {
                let key = key.clone_shared();
                let value = value.clone();
                builder
                    .try_insert_last(key, value)
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
        }
        Ok(Value::Object(
            builder.try_finish().map_err(|_| EngineRunError::allocation_failure())?,
        ))
    }

    /// Materializes one captured value to owned at the construction barrier: an
    /// already-owned value passes through; a located value materializes through the
    /// authoritative `Document`.
    pub(crate) fn materialize_capture(
        &mut self,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Value, EngineRunError> {
        self.materialize_operand(value, resources)
    }

    /// Materializes the SUBJECT of an answering builtin — and, since 061
    /// Family 1, the ROOT of a read-only path-family or pointer call —
    /// memoized by the located input's node handle.
    ///
    /// An answering builtin (`index`, `contains`, `has`, ...) materializes its
    /// whole subject per call, and a map body probing a bound array (`$b |
    /// index($v)`) hands the SAME located node to every call — without the
    /// memo that is one O(subject) deep materialization per probe. The memo
    /// turns the run
    /// into one materialization plus O(1) share-returns, which is sound
    /// because a document is immutable: the same (document, node) always
    /// materializes to the same owned value, and the value's shadow-byte
    /// residency is the run-lifetime capture charge the first materialization
    /// already retained. `reseed` clears the entry with the charges, so a memo
    /// handle can never outlive the charge its bytes stand on.
    ///
    /// The memo deliberately does NOT live in [`Self::materialize_capture`]:
    /// the ARGUMENT answer path also calls that, and its located nodes change
    /// per element (the needle `[$v]` of a fresh element), so a shared one-entry
    /// memo would thrash and the subject would re-materialize per call — the
    /// exact failure the memo exists to prevent. The subject key is the stable
    /// one; the argument's materialization is small and stays uncached. The
    /// same stability is why the path-family and pointer calls may use it:
    /// their input node is fixed for the whole loop, and the memoized root is
    /// only ever read (write families keep the un-memoized
    /// [`Self::materialize_capture`] so their root stays exclusively owned).
    pub(crate) fn materialize_subject_memoized(
        &mut self,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Value, EngineRunError> {
        if let EngineResult::Located(located) = &value {
            let node = located.node();
            if let Some((cached_node, cached)) = &self.memo
                && *cached_node == node
            {
                return Ok(cached.clone());
            }
        }
        // The node key is captured BEFORE the operand move: `value` is consumed
        // by the materialization, and the key must survive it.
        let memo_key = match &value {
            EngineResult::Located(located) => Some(located.node()),
            EngineResult::Owned(_) => None,
        };
        let owned = self.materialize_capture(value, resources)?;
        if let Some(node) = memo_key {
            self.memo = Some((node, owned.clone()));
        }
        Ok(owned)
    }

    /// Materializes one value to owned and HANDS BACK its shadow-byte residency,
    /// leaving the scope to the caller: the charge lives exactly as long as the
    /// holder of the owned bytes does.
    ///
    /// `None` means the caller owes nothing. An already-owned value was charged
    /// where it was bought. A value whose owned form was PROMOTED into a live
    /// fan-out frame keeps the run-lifetime charge taken here, because
    /// [`Self::promote_descent_frame`] shares that container with the frame — the
    /// only path on which a materialized value outlives the expression that asked
    /// for it.
    pub(crate) fn materialize_operand(
        &mut self,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Value, EngineRunError> {
        let promotable = match &value {
            EngineResult::Owned(_) => None,
            EngineResult::Located(located) => self.promotable_descent_frame(located),
        };
        let owned = self.materialize_uncharged(value, resources)?;
        if let Some(frame) = promotable {
            self.promote_descent_frame(frame, &owned)?;
        }
        Ok(owned)
    }

    /// Reports the innermost live fan-out frame whose container is EXACTLY the
    /// located value a barrier is about to materialize, if there is one.
    ///
    /// This is the eligibility half of container promotion (see
    /// [`Self::promote_descent_frame`]). It is a capability question about the
    /// machine's own state — "the value being materialized is a container this
    /// run is still iterating" — and names no program shape: a `..` under any
    /// barrier answers yes, a `.[]` frame answers no because a `.[]` barrier
    /// captures CHILDREN and never the container, and a residual between the
    /// descent and the barrier answers no because the captured value is then not
    /// the container.
    ///
    /// Only the INNERMOST fan-out frame is considered. The `..` walk is pre-order
    /// self-first — the frame is pushed and the self-emission routed before any
    /// child advances — so the innermost frame is the only one whose container
    /// can be the value in flight. Scanning further would let a DESCENDANT's
    /// materialization masquerade as an ancestor's.
    ///
    /// Identity is `NodeHandle` equality alone, and that is sufficient rather
    /// than convenient: a handle carries its `DocumentId` — a process-unique
    /// `DocumentId` drawn from a counter that FAILS rather than wraps, plus the
    /// revision — so two equal handles name the same node of the same document
    /// even when they arrived from unrelated products. A comparison of the
    /// document allocations beside it would restate what the handle already
    /// proves.
    pub(crate) fn promotable_descent_frame(&self, located: &LocatedProduct<'source>) -> Option<usize> {
        let (index, frame) = self
            .frames
            .as_slice()
            .iter()
            .enumerate()
            .rev()
            .find(|(_, frame)| matches!(frame, GraphFrame::Each { .. }))?;
        let GraphFrame::Each {
            container: Container::Located(container),
            ..
        } = frame
        else {
            return None;
        };
        (container.node() == located.node()).then_some(index)
    }

    /// Replaces a fan-out frame's LOCATED container with the owned value a
    /// barrier just materialized from that same node, so the frame's remaining
    /// children are shared out of it instead of re-walked from the document.
    ///
    /// This is the deep-collect mechanism, and the cost it removes is
    /// quadratic. `..` emits every node of a document, and a construction barrier
    /// materializes each emission WHOLE: for `[..]` over the 10 MB catalog that is
    /// 4,032,010 owned nodes built for 864,004 logical ones, because every
    /// ancestor re-materializes its entire subtree. The promoted frame turns the
    /// 863,996 descendant materializations into `Arc` bumps — the parent's owned
    /// form already contains every child, and `Value::clone` shares it.
    ///
    /// Nothing is built speculatively. The owned container is not materialized
    /// FOR the frame; it is the value the barrier had to build anyway, retained
    /// by one snapshot clone. A run whose barrier never captures a container
    /// (`reduce .. as $x`, which binds located handles) promotes nothing and pays
    /// nothing.
    ///
    /// One decline, by capability rather than by shape: a non-aggregate owned
    /// form is refused because there is nothing to share and the fan-out's
    /// owned-child navigation is defined over arrays and objects only. A `Tagged`
    /// container is NOT refused: the shared-backing argument survives the tag
    /// wrapper — the payload is a `Shared<Value>`, so [`Value::try_clone`]
    /// refcount-bumps it and the promoted frame shares every child out of the
    /// tagged payload's own storage — and `next_owned_child` untags before it
    /// iterates.
    ///
    /// Reports whether the frame's container was replaced. That answer is what
    /// tells a caller its materialized bytes now outlive the expression that
    /// built them, so the charge covering them must be run-lifetime.
    pub(crate) fn promote_descent_frame(&mut self, frame: usize, owned: &Value) -> Result<bool, EngineRunError> {
        if !matches!(owned.untagged(), Value::Array(_) | Value::Object(_)) {
            return Ok(false);
        }
        let shared = owned.clone();
        let Some(GraphFrame::Each { container, .. }) = self.frames.as_mut_slice().get_mut(frame) else {
            return Err(internal_contract("promoted frame kind changed under capture"));
        };
        *container = Container::Owned(shared);
        Ok(true)
    }

    /// Materializes one value to owned WITHOUT taking a run-lifetime residency:
    /// an already-owned value passes through, a located one materializes through
    /// the authoritative `Document` (growing the shared workspace, whose growth is
    /// itself bracketed once per run).
    ///
    /// The caller decides the accounting: the construction barrier holds a
    /// run-lifetime charge ([`Self::materialize_capture`]), while an env slot and
    /// a loop state hold a residency released on their next overwrite.
    ///
    /// When a path register is sitting on this located node, the rematerialization
    /// reuses the register's owned address instead of building a twin. Law 1 is
    /// allocation identity: a reduce-of-dot, empty-path getpath, or recurse-self
    /// emission IS the register address even though the body stayed located.
    pub(crate) fn materialize_uncharged(
        &mut self,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Value, EngineRunError> {
        let located = match value {
            EngineResult::Owned(owned) => return Ok(owned),
            EngineResult::Located(located) => located,
        };
        if let Some(shared) = self
            .path
            .as_ref()
            .and_then(|register| register.owned_for_node(located.node()))
        {
            return Ok(shared);
        }
        let document = located.product().document();
        self.reserve_workspace(document.node_count(), resources);
        let workspace = self
            .workspace
            .as_mut()
            .ok_or_else(|| internal_contract("materializer workspace missing after reservation"))?;
        document
            .materialize_node_with(workspace, located.node(), resources)
            .map_err(|_| internal_contract("materialization at the construction barrier failed"))
    }

    /// Ensures the run's reusable materializer workspace exists; the bitmap
    /// itself grows inside the materialize call.
    pub(crate) fn reserve_workspace(&mut self, _node_count: usize, _resources: &mut ResourceContext<'_>) {
        self.workspace.get_or_insert_with(MaterializeWorkspace::new);
    }

    /// Advances the top continuation frame: emits the next `Each` child, fires a
    /// `Choice` right member (popping the frame and taking its retained input), or
    /// pops an exhausted frame. Returns `Ok(false)` when no frames remain (the
    /// stream is complete).
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per frame family: the top-of-stack law is read as a single table"
    )]
    pub(crate) fn advance_frame(&mut self, resources: &mut ResourceContext<'_>) -> Result<bool, EngineRunError> {
        match self.frames.as_slice().last() {
            None => return Ok(false),
            // A Choice reaching the top means its left member is fully done: fire
            // the right member over the retained input, popping the frame.
            Some(GraphFrame::Choice { .. }) => {
                let Some(GraphFrame::Choice { right, input, cont }) = self.frames.pop() else {
                    return Err(internal_contract("graph frame kind changed under pop"));
                };
                self.pending = Some(GraphTask::Eval {
                    node: right,
                    input,
                    cont,
                });
                return Ok(true);
            }
            Some(GraphFrame::BindBody { collected, .. }) if !collected.is_empty() => {
                let step = {
                    let frame = self.frames.as_mut_slice().last_mut().ok_or_else(|| internal_contract("bind body frame vanished under replay"))?;
                    let GraphFrame::BindBody {
                        slot,
                        body,
                        input,
                        out_cont,
                        collected,
                        cursor,
                        ..
                    } = frame
                    else {
                        return Err(internal_contract("bind body frame kind changed under replay"));
                    };
                    if *cursor >= collected.len() {
                        None
                    } else {
                        let first = *cursor == 0;
                        let value = collected[*cursor].clone();
                        *cursor += 1;
                        let input = input.try_clone().map_err(EngineRunError::Codec)?;
                        Some((*slot, *body, input, *out_cont, value, first))
                    }
                };
                let Some((slot, body, input, out_cont, value, first)) = step else {
                    self.frames.pop();
                    return Ok(true);
                };
                // The source's frozen scope ends before the FIRST body runs.
                if first {
                    self.path_exit_frozen();
                }
                self.write_slot(slot, EngineResult::owned(value))?;
                self.pending = Some(GraphTask::Eval {
                    node: body,
                    input,
                    cont: out_cont,
                });
                return Ok(true);
            }
            // These frames at the top mean their producer is exhausted with nothing
            // more to route — pop and resume the parent: a `FlatMapBody`'s upstream
            // is done, an `ObjectDest`'s members are all done (its objects were
            // routed at their combinations), and an `ObjectKey`'s key producer is
            // done (partial is already at this member's depth).
            Some(
                GraphFrame::FlatMapBody { .. }
                | GraphFrame::ObjectDest { .. }
                | GraphFrame::ObjectKey { .. }
                | GraphFrame::SelectBody { .. }
                // A `BinaryRight` at the top means the right operand is exhausted;
                // a `BinaryLeft` at the top means the left operand is exhausted for
                // the current right output — both pop and resume the parent (the
                // enclosing `BinaryRight`, or the whole `Binary`'s caller).
                | GraphFrame::BinaryRight { .. }
                | GraphFrame::BinaryLeft { .. }
                | GraphFrame::ConcatPart { .. }
                // A `MathArgs` at the top means the outer argument being
                // evaluated is exhausted (every output already opened its own
                // nested drive); a `MathCompute` at the top means the first
                // argument is exhausted (every output already published its
                // result) — both pop and resume the parent.
                | GraphFrame::MathArgs { .. }
                | GraphFrame::MathCompute { .. }
                // A `PathSetValue`/`PathSetPair` at the top means that
                // argument is exhausted (every output already opened its own
                // inner drive) — pop and resume the parent, as the math pair.
                | GraphFrame::PathSetValue { .. }
                | GraphFrame::PathSetPair { .. }
                // A `ConditionalBody` at the top means the condition generator is
                // exhausted (every output already selected its arm); a `LogicalLeft`
                // means the left operand is exhausted; a `LogicalRight` means the
                // right operand is exhausted for the current left output — all pop
                // and resume the parent.
                | GraphFrame::ConditionalBody { .. }
                | GraphFrame::LogicalLeft { .. }
                | GraphFrame::LogicalRight { .. }
                // A `Try` at the top means the body finished with no uncaught error
                // — pop it (the body's outputs already emitted through the barrier,
                // the handler never runs). An `ErrorArg` at the top means the
                // `error/1` argument produced zero outputs — pop it
                // (`error(empty)` raises nothing).
                | GraphFrame::Try { .. }
                // A `Label` at the top means the body finished with no matching
                // `break` — pop it, exactly as an uncaught-error-free `Try` pops.
                | GraphFrame::Label { .. }
                | GraphFrame::ErrorArg { .. }
                // An `ArgumentAnswer` at the top means the argument filter is
                // exhausted: every answer it had was already published as it
                // arrived, so the frame simply pops (`bsearch(empty)` publishes
                // nothing at all, not even a rejection).
                | GraphFrame::ArgumentAnswer { .. }
                // A `BindBody` at the top means the source is exhausted; a
                // loop INIT frame means the init generator is exhausted; a
                // `ForeachSource` means the source is exhausted with nothing
                // further to publish — all pop. A binder frame carries no
                // restore duty: its slot is unique to this occurrence.
                | GraphFrame::BindBody { .. }
                | GraphFrame::ReduceInit { .. }
                | GraphFrame::ForeachInit { .. }
                | GraphFrame::ForeachSource { .. }
                // A loop UPDATE frame at the top means the update generator is
                // exhausted for this source item. The state already holds the LAST
                // update output, or stayed `null` when the update emitted nothing
                // (it was taken out of the source frame when the item began).
                | GraphFrame::ReduceUpdate { .. }
                | GraphFrame::ForeachUpdate { .. }
                // A keyed-collect KEY frame at the top means the key generator
                // is exhausted for this item — pop it and resume the source
                // (every key it emitted already landed in the builder).
                | GraphFrame::KeyedCollectKey { .. }
                // A `Counted` at the top means the source ran out before the
                // count did — pop it. A count that ran out first popped this
                // frame itself, at the cut.
                | GraphFrame::Counted { .. }
                // An `EngineBindBody` at the top means the binding body's single
                // evaluation is complete: pop it and RELEASE the cursor (RAII —
                // scope exit closes).
                | GraphFrame::EngineBindBody { .. },
            ) => {
                // A spent `Try` here brackets a body that finished with no
                // uncaught error — its suppression region ends with it. The
                // `AlternativeLeft` arm below pops its own.
                if matches!(
                    self.frames.as_slice().last(),
                    Some(GraphFrame::Try { .. })
                ) {
                    resources.exit_mismatch_suppression();
                }
                let popped = self.frames.pop();
                // The RAII release rides every pop path of the binding barrier.
                if let Some(GraphFrame::EngineBindBody { slot, .. }) = popped {
                    self.release_engine_cursor(slot);
                }
                // A pop-exit frozen drive (a collect/object/argument frame)
                // ends its scope at its normal completion pop.
                if let Some(popped) = &popped
                    && path_register::frozen_pop_frame(popped)
                {
                    self.path_exit_frozen();
                }
                return Ok(true);
            }
            // A `PathPublish` at the top means the path body completed:
            // restore the displaced register and hand the collected paths to
            // their consumer (`path(f)` streams via `PathEmit`; assignments
            // go to their fold).
            Some(GraphFrame::PathPublish { .. }) => {
                let Some(GraphFrame::PathPublish {
                    saved,
                    values,
                    pending,
                    out_cont,
                    finish,
                    assign,
                    ..
                }) = self.frames.pop()
                else {
                    return Err(internal_contract("path publish frame kind changed under pop"));
                };
                self.path = saved;
                if let PathFinish::Stream = finish {
                    self.push_frame(GraphFrame::PathEmit {
                        values,
                        cursor: 0,
                        pending,
                        out_cont,
                    });
                } else {
                    self.finish_assignment_paths(values, pending, assign, resources)?;
                }
                return Ok(true);
            }
            // A `PathStep` marker's navigation completed: pop it and restore
            // the register, so a sibling sees it pre-navigation.
            Some(GraphFrame::PathStep { .. }) => {
                let Some(GraphFrame::PathStep {
                    steps_len,
                    at,
                    at_node,
                }) = self.frames.pop()
                else {
                    return Err(internal_contract("path step frame kind changed under pop"));
                };
                if let Some(register) = self.path.as_mut() {
                    register.restore_navigation(steps_len, at, at_node);
                }
                return Ok(true);
            }
            // A register-active `BindBody` with collected outputs replays
            // them as bodies. A `PathEmit` publishes its next value, popping
            // when exhausted; it is mutated IN PLACE (a re-push would need a
            // resource context).
            Some(GraphFrame::PathEmit { .. }) => {
                let top = self.frames.as_slice().len() - 1;
                let Some(GraphFrame::PathEmit {
                    values,
                    cursor,
                    pending,
                    out_cont,
                }) = self.frames.as_mut_slice().get_mut(top)
                else {
                    return Err(internal_contract("graph frame kind changed under advance"));
                };
                if *cursor >= values.len() {
                    // The prefix is spent: a stored failure raises HERE,
                    // from the call's own out-continuation, so the enclosure
                    // test sees it at the call's position.
                    let failure = pending.take();
                    let out_cont = *out_cont;
                    self.frames.pop();
                    if let Some(failure) = failure {
                        self.raise_cont = out_cont;
                        return Err(failure);
                    }
                    return Ok(true);
                }
                let value = core::mem::replace(&mut values[*cursor], Value::Null);
                *cursor += 1;
                let out_cont = *out_cont;
                self.pending = Some(GraphTask::Route {
                    value: EngineResult::owned(value),
                    cont: out_cont,
                });
                return Ok(true);
            }
            // An `InputsEmit` at the top pulls ONE value from the shared input
            // source and routes it, popping at end of stream. The pull happens
            // per resumption, so a countdown cut between two resumptions drops
            // the frame with no further value pulled — and, on a streaming
            // host cursor, no further input bytes read. A parse refusal raises
            // the message text from the `inputs` call's own out-continuation,
            // catch-eligible, with the published prefix standing.
            Some(GraphFrame::InputsEmit { out_cont }) => {
                let out_cont = *out_cont;
                let pull = with_input_source(resources, |source, resources| {
                    let pulled = source.next(resources);
                    if matches!(pulled, Ok(Some(_))) {
                        source.mark_current();
                    }
                    pulled
                });
                match pull {
                    Some(Ok(Some(value))) => {
                        // The pulled value becomes the CURRENT input, exactly
                        // as `input`'s single pull marks it:
                        // `input_line_number` and `input_filename` describe the
                        // last value READ, whichever builtin read it (`jq
                        // '[inputs] | length | input_line_number'` over
                        // `1\n2\n3\n` answers 3, the last line — jqf answered 1,
                        // the driver's, because only `input` was marking).
                        self.pending = Some(GraphTask::Route {
                            value: EngineResult::owned(value),
                            cont: out_cont,
                        });
                    }
                    Some(Ok(None)) | None => {
                        self.frames.pop();
                    }
                    Some(Err(InputSourceError::Refused(message))) => {
                        let raised = Value::try_string(&message)
                            .map_err(|_| EngineRunError::allocation_failure())?;
                        self.frames.pop();
                        self.raise_cont = out_cont;
                        return Err(EngineRunError::Raised(raised));
                    }
                    Some(Err(InputSourceError::Allocation)) => {
                        self.frames.pop();
                        return Err(EngineRunError::allocation_failure());
                    }
                }
                return Ok(true);
            }
            // A `TostreamEmit` at the top advances the resumable walk ONE
            // emission and routes it, popping when the walk is spent. The
            // frame is mutated in place like `PathEmit`; a cut between
            // resumptions drops the walk with it, so `limit(1; tostream)`
            // computes one item — never the whole stream.
            Some(GraphFrame::TostreamEmit { .. }) => {
                let top = self.frames.as_slice().len() - 1;
                let (emitted, out_cont) = {
                    let GraphFrame::TostreamEmit { walk, out_cont } =
                        self.frames.as_mut_slice().get_mut(top).ok_or_else(|| internal_contract("graph frame vanished under advance"))?
                    else {
                        return Err(internal_contract("graph frame kind changed under advance"));
                    };
                    let out_cont = *out_cont;
                    let emitted = walk.next(resources)?;
                    (emitted, out_cont)
                };
                match emitted {
                    Some(value) => {
                        self.pending = Some(GraphTask::Route {
                            value: EngineResult::owned(value),
                            cont: out_cont,
                        });
                    }
                    None => {
                        self.frames.pop();
                    }
                }
                return Ok(true);
            }
            // A `SelectorEmit` at the top publishes its next located element,
            // and pops when the set is exhausted. The frame is mutated in
            // place exactly as `PathEmit` is; a pending failure is raised
            // from the selector call's own out-continuation after the prefix
            // has been routed.
            Some(GraphFrame::SelectorEmit { .. }) => {
                let top = self.frames.as_slice().len() - 1;
                let exhausted = {
                    let frame = self.frames.as_mut_slice().get_mut(top).ok_or_else(|| internal_contract("selector frame vanished under advance"))?;
                    let GraphFrame::SelectorEmit {
                        results,
                        pending,
                        out_cont,
                        ..
                    } = frame
                    else {
                        return Err(internal_contract("selector frame kind changed under advance"));
                    };
                    if results.is_empty() {
                        Some((pending.take(), *out_cont))
                    } else {
                        None
                    }
                };
                if let Some((failure, out_cont)) = exhausted {
                    self.frames.pop();
                    if let Some(failure) = failure {
                        self.raise_cont = out_cont;
                        return Err(failure);
                    }
                    return Ok(true);
                }
                let (input, value, out_cont) = {
                    let frame = self.frames.as_mut_slice().get_mut(top).ok_or_else(|| internal_contract("selector frame vanished under advance"))?;
                    let GraphFrame::SelectorEmit {
                        input,
                        results,
                        out_cont,
                        ..
                    } = frame
                    else {
                        return Err(internal_contract("selector frame kind changed under advance"));
                    };
                    let result = results
                        .pop_front()
                        .ok_or_else(|| internal_contract("selector results emptied between polls"))?;
                    (
                        input.try_clone().map_err(EngineRunError::Codec)?,
                        result,
                        *out_cont,
                    )
                };
                let value = match value {
                    SelectorEmitValue::Node(node) => {
                        let located_ref = input.located().ok_or_else(|| internal_contract("selector frame without located authority"))?;
                        selector_located_node(located_ref, node, self, resources)?
                    }
                    SelectorEmitValue::Owned(value) => {
                        self.retain_owned(value)
                    }
                };
                self.pending = Some(GraphTask::Route {
                    value,
                    cont: out_cont,
                });
                return Ok(true);
            }

            // A `PathWalkEmit` at the top advances the resumable walk: drive
            // the per-child body machine, walk to the next child, or pop when
            // the walk is exhausted. It is mutated IN PLACE like `PathEmit`,
            // and its nested body machines are dropped with the frame when a
            // countdown cut truncates the stack — which is the whole point:
            // the walk dies where the cut fired.
            Some(GraphFrame::PathWalkEmit { .. }) => {
                let top = self.frames.as_slice().len() - 1;
                return self.advance_path_walk_emit(top, resources);
            }
            // A `CallableBody` at the top drives the lazy recursive-callable
            // producer: resume the nested machine to its next output, or start
            // the next argument combination's machine, or — when every
            // combination is exhausted — pop (the call has produced all of its
            // outputs). Each resumption routes ONE output, so a countdown cut
            // between two resumptions drops this frame and the child machine
            // with it, and the recursion dies where the cut fired.
            Some(GraphFrame::CallableBody { .. }) => {
                let Some(GraphFrame::CallableBody {
                    body,
                    root,
                    overlay,
                    param_slots,
                    combos,
                    mut combo,
                    depth,
                    mut child,
                    out_cont,
                    filter_env,
                }) = self.frames.pop()
                else {
                    return Err(internal_contract("callable frame kind changed under pop"));
                };
                loop {
                    // Re-arms the popped frame with its advanced state; every
                    // resumption path below ends in exactly one call. A macro,
                    // not a closure: each expansion moves the frame's state on
                    // its own mutually exclusive return path.
                    macro_rules! rearm {
                        ($me:expr, $combo:expr, $child:expr) => {{
                            $me.push_frame(GraphFrame::CallableBody {
                                body,
                                root,
                                overlay,
                                param_slots,
                                combos,
                                combo: $combo,
                                depth,
                                child: $child,
                                out_cont,
                                filter_env,
                            });
                        }};
                    }
                    let mut machine = if let Some(mut machine) = child.take() {
                        // A completion-drain that over-produced parks the
                        // extra item on the machine; publishing it IS this
                        // resumption — no advance runs past it.
                        if let Some(queued) = machine.queued_output.take() {
                            self.adopt_child_fact_deltas(&machine);
                            if machine.path.is_some() {
                                self.path = machine.path.clone();
                            }
                            self.pending = Some(GraphTask::Route {
                                value: queued,
                                cont: out_cont,
                            });
                            rearm!(self, combo, Some(machine));
                            return Ok(true);
                        }
                        machine
                    } else {
                        if combo >= combos.len() {
                            self.recycle_slot_scratch(overlay);
                            return Ok(true);
                        }
                        let mut machine =
                            self.take_body_machine(body, EngineResult::owned(root.clone()))?;
                        // The callable body is part of the enclosing path
                        // drive: the nested machine is seeded with a register
                        // clone and the parent adopts it at every item.
                        machine.path.clone_from(&self.path);
                        machine.callable_depth = depth;
                        machine.filter_env.clone_from(&filter_env);
                        // A pooled machine keeps its previous call's env;
                        // null every cell the new overlay leaves unbound so
                        // the reuse reads exactly as a fresh seed would
                        // (the same load law the argument pool uses).
                        for (index, value) in overlay.iter().enumerate() {
                            match value {
                                Some(value) => {
                                    machine.write_slot(
                                        u32::try_from(index).map_err(|_| {
                                            internal_contract(
                                                "callable overlay slot index overflow",
                                            )
                                        })?,
                                        EngineResult::owned(value.clone()),)?;
                                }
                                None => {
                                    if let Some(cell) = machine.env.get_mut(index) {
                                        *cell = EngineResult::owned(Value::Null);
                                    }
                                }
                            }
                        }
                        if overlay.len() < machine.env.len() {
                            for cell in &mut machine.env[overlay.len()..] {
                                *cell = EngineResult::owned(Value::Null);
                            }
                        }
                        for (slot, value) in param_slots.iter().zip(&combos[combo]) {
                            machine.write_slot(
                                *slot,
                                EngineResult::owned(value.clone()),)?;
                        }
                        machine
                    };
                    machine.inherit_fact_deltas(self);
                    match machine.advance(resources) {
                        Ok(RunPoll::Item(item)) => {
                            self.adopt_child_fact_deltas(&machine);
                            // The nested drive owns the register while it
                            // runs; the parent ADOPTS it before routing, so
                            // the publish check sees the callable's
                            // navigations.
                            if machine.path.is_some() {
                                self.path = machine.path.clone();
                            }
                            // A ONE-SHOT body: nothing is left to drive (no
                            // pending task, no frames), so this item was the
                            // body's LAST output and the frame's only
                            // remaining duty is a vacuous Complete poll.
                            // Pool the machine NOW — the next combo's seed
                            // (or the next call admission on this machine)
                            // reuses it — and advance the combo exactly as
                            // the Complete arm does.
                            if machine.pending.is_none() && machine.frames.is_empty() {
                                self.put_body_machine(machine);
                                self.pending = Some(GraphTask::Route {
                                    value: item,
                                    cont: out_cont,
                                });
                                rearm!(self, combo + 1, None);
                                return Ok(true);
                            }
                            // Otherwise try ONE completion-drain advance —
                            // NEVER under an active path register: the
                            // register an item publishes through is the one
                            // as-of its production, and a drain advance would
                            // mutate it before the publish. A body whose
                            // remaining frames are drained producers answers
                            // Complete, and pooling NOW — instead of after the
                            // parent's next take — is what lets sibling call
                            // admissions reuse it. A Pending keeps the
                            // ordinary resumption path; a second Item (a
                            // multi-output body) parks in the machine's
                            // one-deep queue for the frame's next resumption,
                            // keeping one-output-per-resumption (and the
                            // countdown cuts that rely on it) intact.
                            if machine.path.is_none() {
                                match machine.advance(resources) {
                                    Ok(RunPoll::Complete) => {
                                        self.adopt_child_fact_deltas(&machine);
                                        self.put_body_machine(machine);
                                        self.pending = Some(GraphTask::Route {
                                            value: item,
                                            cont: out_cont,
                                        });
                                        rearm!(self, combo + 1, None);
                                        return Ok(true);
                                    }
                                    Ok(RunPoll::Pending) => {
                                        self.adopt_child_fact_deltas(&machine);
                                        self.pending = Some(GraphTask::Route {
                                            value: item,
                                            cont: out_cont,
                                        });
                                        rearm!(self, combo, Some(machine));
                                        return Ok(true);
                                    }
                                    Ok(RunPoll::Item(extra)) => {
                                        machine.queued_output = Some(extra);
                                        self.adopt_child_fact_deltas(&machine);
                                        self.pending = Some(GraphTask::Route {
                                            value: item,
                                            cont: out_cont,
                                        });
                                        rearm!(self, combo, Some(machine));
                                        return Ok(true);
                                    }
                                    Err(error) => {
                                        self.adopt_child_fact_deltas(&machine);
                                        self.raise_cont = out_cont;
                                        return Err(error);
                                    }
                                }
                            }
                            // Path-context body: the ordinary resumption
                            // path verbatim.
                            self.pending = Some(GraphTask::Route {
                                value: item,
                                cont: out_cont,
                            });
                            rearm!(self, combo, Some(machine));
                            return Ok(true);
                        }
                        Ok(RunPoll::Complete) => {
                            self.adopt_child_fact_deltas(&machine);
                            self.put_body_machine(machine);
                            combo += 1;
                        }
                        Ok(RunPoll::Pending) => {
                            self.adopt_child_fact_deltas(&machine);
                            // The child's slice ran out mid-drive: put it back
                            // exactly as the Item arm does and let this loop's
                            // NEXT admission surface the pause (the parent's
                            // own admission was consumed before the task ran,
                            // so the next iteration observes the exhaustion).
                            self.push_frame(
                                GraphFrame::CallableBody {
                                    body,
                                    root,
                                    overlay,
                                    param_slots,
                                    combos,
                                    combo,
                                    depth,
                                    child: Some(machine),
                                    out_cont,
                                    filter_env,
                                },
                            );
                            return Ok(true);
                        }
                        Err(error) => {
                            self.adopt_child_fact_deltas(&machine);
                            // The body's failure is the CALL's failure: it
                            // routes at the call's own out-continuation,
                            // exactly where an eagerly evaluated body's raise
                            // routed.
                            self.raise_cont = out_cont;
                            return Err(error);
                        }
                    }
                }
            }
            // A `ReduceSource` at the top means the source is exhausted: pop it and
            // emit the ONE final state (an empty source emits the init unchanged).
            Some(GraphFrame::ReduceSource { .. }) => {
                let Some(GraphFrame::ReduceSource {
                    mut state,
                    out_cont,
                    ..
                }) = self.frames.pop()
                else {
                    return Err(internal_contract("graph frame kind changed under pop"));
                };
                self.pending = Some(GraphTask::Route {
                    value: EngineResult::owned(state.take()),
                    cont: out_cont,
                });
                return Ok(true);
            }
            // An `EnginePull` at the top drives the suspended cursor machine one
            // resumption — the `CallableBody` drive shape: each resumption
            // routes ONE value, a `.next` pull pops after it (ONE value per
            // evaluation), a `.rest` pull re-pushes itself until the cursor
            // completes. A countdown cut between two resumptions drops this
            // frame and the binding barrier's release with it.
            Some(GraphFrame::EnginePull { .. }) => {
                let Some(GraphFrame::EnginePull {
                    slot,
                    rest,
                    out_cont,
                }) = self.frames.pop()
                else {
                    return Err(internal_contract("engine pull frame kind changed under pop"));
                };
                let machine = self.cursors.get_mut(slot.index());
                let Some(machine) = machine else {
                    // Defense in depth: a nested evaluator (a `paths`/`path`
                    // body machine, or a future one) that owns no cursor
                    // store raises a DEFINED error — never a hang, never a
                    // silent wrong answer. Lower time rejects the reachable
                    // cases (recursive defs, constructor capture).
                    self.raise_cont = out_cont;
                    return Err(engine_binding_unavailable(resources));
                };
                let Some(machine) = machine.as_mut() else {
                    self.raise_cont = out_cont;
                    return Err(engine_binding_unavailable(resources));
                };
                // Inherit before this poll: `machine` mut-borrows `self.cursors`,
                // so clone the parent overlay first rather than calling
                // `inherit_fact_deltas`.
                let parent_deltas = self.fact_deltas.clone();
                machine.fact_deltas.clone_from(&parent_deltas);
                let poll = machine.advance(resources);
                self.fact_deltas.clone_from(&machine.fact_deltas);
                return match poll {
                    Ok(RunPoll::Item(item)) => {
                        self.pending = Some(GraphTask::Route {
                            value: item,
                            cont: out_cont,
                        });
                        if rest {
                            // `.rest`: keep pulling — the frame stays for
                            // the next resumption.
                            self.push_frame(
                                GraphFrame::EnginePull {
                                    slot,
                                    rest,
                                    out_cont,
                                },
                            );
                        }
                        Ok(true)
                    }
                    Ok(RunPoll::Complete) => {
                        // Exhaustion is idempotent: the cursor machine's
                        // own `done` law makes every later poll `Complete`.
                        // A `.next` pull here emits NOTHING (empty); a
                        // `.rest` pull is spent.
                        Ok(true)
                    }
                    Ok(RunPoll::Pending) => {
                        // The cursor's slice ran out mid-drive: put the
                        // frame back and let this loop's NEXT admission
                        // surface the pause (the parent's admission was
                        // consumed before this task ran).
                        self.push_frame(
                            GraphFrame::EnginePull {
                                slot,
                                rest,
                                out_cont,
                            },
                        );
                        Ok(true)
                    }
                    Err(error) => {
                        // The cursor's failure is the PULL's failure, routed
                        // at the pull's own out-continuation.
                        self.raise_cont = out_cont;
                        Err(error)
                    }
                };
            }
            // An `EngineRng` at the top: the `Seed` phase's graph completed —
            // a seed that produced nothing exhausts the cursor forever (the
            // empty-init law) — or the `Emit` phase routes one drawn float.
            Some(GraphFrame::EngineRng { .. }) => {
                let top = self.frames.as_slice().len() - 1;
                let phase = match self.frames.as_slice().get(top) {
                    Some(GraphFrame::EngineRng { phase, .. }) => *phase,
                    _ => return Err(internal_contract("engine rng frame kind changed under advance"))
                };
                match phase {
                    EngineRngPhase::Seed => {
                        // The seed completed with zero outputs: the cursor is
                        // exhausted forever — POP the frame so the machine
                        // reaches its `done` state (an empty generator init
                        // follows the same law).
                        self.frames.pop();
                        return Ok(true);
                    }
                    EngineRngPhase::Emit => {
                        let (out_cont, value) = {
                            let frame = self.frames.as_mut_slice().get_mut(top).ok_or_else(|| internal_contract("engine rng frame vanished"))?;
                            let GraphFrame::EngineRng {
                                rng, out_cont, ..
                            } = frame
                            else {
                                return Err(internal_contract("engine rng frame kind changed under advance"));
                            };
                            let rng = rng.as_mut().ok_or_else(|| internal_contract("engine rng emit phase has no generator"))?;
                            (*out_cont, rand_float(rng))
                        };
                        self.pending = Some(GraphTask::Route {
                            value: EngineResult::owned(value),
                            cont: out_cont,
                        });
                        return Ok(true);
                    }
                }
            }
            // An `EngineGen` at the top means the current phase's graph is
            // exhausted: advance the `~generator` drive — init completes into
            // the first update (or exhausts the cursor for an empty init),
            // update completes into extract (or exhausts the cursor for an
            // empty update), extract completes back into the next update (an
            // empty-extract cycle emits nothing and continues).
            Some(GraphFrame::EngineGen { .. }) => {
                let top = self.frames.as_slice().len() - 1;
                let (phase, node) = match self.frames.as_slice().get(top) {
                    Some(GraphFrame::EngineGen {
                        phase, node, ..
                    }) => (*phase, *node),
                    _ => return Err(internal_contract("engine generator frame kind changed under advance"))
                };
                let ProgramNode::EngineGenerator {
                    init,
                    update,
                    extract,
                } = &self.nodes[node.index()]
                else {
                    return Err(internal_contract("engine generator drive lost its generator node"));
                };
                let (_init, update, extract) = (*init, *update, *extract);
                match phase {
                    EngineGenPhase::Init => {
                        let state = {
                            let frame = self.frames.as_mut_slice().get_mut(top).ok_or_else(|| internal_contract("engine generator frame vanished"))?;
                            let GraphFrame::EngineGen {
                                state, phase, ..
                            } = frame
                            else {
                                return Err(internal_contract("engine generator frame kind changed under advance"));
                            };
                            *phase = EngineGenPhase::Update;
                            state.take()
                        };
                        let Some(mut state) = state else {
                            // An EMPTY init: the cursor is exhausted forever.
                            self.frames.pop();
                            return Ok(true);
                        };
                        self.pending = Some(GraphTask::Eval {
                            node: update,
                            input: EngineResult::owned(state.take()),
                            cont: top + 1,
                        });
                        return Ok(true);
                    }
                    EngineGenPhase::Update => {
                        // An EMPTY update EXHAUSTS the cursor forever: no state
                        // was produced this cycle, and no later cycle exists to
                        // continue from.
                        let has_state = {
                            let frame = self.frames.as_mut_slice().get_mut(top).ok_or_else(|| internal_contract("engine generator frame vanished"))?;
                            let GraphFrame::EngineGen { state, .. } = frame else {
                                return Err(internal_contract("engine generator frame kind changed under advance"));
                            };
                            state.is_some()
                        };
                        if !has_state {
                            self.frames.pop();
                            return Ok(true);
                        }
                        // The extract runs over a CLONE of the new state; the
                        // frame keeps the state (and its residency) for the
                        // NEXT cycle's update.
                        let extract_input = {
                            let frame = self.frames.as_mut_slice().get_mut(top).ok_or_else(|| internal_contract("engine generator frame vanished"))?;
                            let GraphFrame::EngineGen {
                                state,
                                phase,
                                emitted,
                                ..
                            } = frame
                            else {
                                return Err(internal_contract("engine generator frame kind changed under advance"));
                            };
                            *phase = EngineGenPhase::Extract;
                            *emitted = false;
                            let cell = state.as_mut().ok_or_else(|| internal_contract("engine generator update produced no state"))?;
                            EngineResult::owned(cell.value.clone())
                        };
                        self.pending = Some(GraphTask::Eval {
                            node: extract,
                            input: extract_input,
                            cont: top + 1,
                        });
                        return Ok(true);
                    }
                    EngineGenPhase::Extract => {
                        // The extract phase is exhausted: this cycle is done,
                        // whether it emitted or not. Start the next cycle's
                        // update over the state the frame kept.
                        let state = {
                            let frame = self.frames.as_mut_slice().get_mut(top).ok_or_else(|| internal_contract("engine generator frame vanished"))?;
                            let GraphFrame::EngineGen {
                                state,
                                phase,
                                emitted,
                                ..
                            } = frame
                            else {
                                return Err(internal_contract("engine generator frame kind changed under advance"));
                            };
                            *phase = EngineGenPhase::Update;
                            let _ = emitted;
                            state.take()
                        };
                        let Some(mut state) = state else {
                            // The state was already consumed (an empty update
                            // took it and found nothing): exhausted.
                            self.frames.pop();
                            return Ok(true);
                        };
                        self.pending = Some(GraphTask::Eval {
                            node: update,
                            input: EngineResult::owned(state.take()),
                            cont: top + 1,
                        });
                        return Ok(true);
                    }
                }
            }
            // A `KeyedCollectSource` at the top means the source is exhausted:
            // finish the builder into ONE object and route it once (an empty
            // source finishes an empty object — the empty-init law, without
            // ever materializing the init).
            Some(GraphFrame::KeyedCollectSource { .. }) => {
                let Some(GraphFrame::KeyedCollectSource {
                    builder,
                    out_cont,
                    ..
                }) = self.frames.pop()
                else {
                    return Err(internal_contract("graph frame kind changed under pop"));
                };
                let object = builder.try_finish().map_err(|_| EngineRunError::allocation_failure())?;
                self.pending = Some(GraphTask::Route {
                    value: EngineResult::owned(Value::Object(object)),
                    cont: out_cont,
                });
                return Ok(true);
            }
            // An `AlternativeLeft` at the top means the left generator is exhausted:
            // pop it, and if no truthy output ever passed through, seed the right
            // fallback once over the retained original input (routed through the
            // alternative's out-continuation). If a truthy output was seen, the
            // fallback never runs.
            Some(GraphFrame::AlternativeLeft { .. }) => {
                let Some(GraphFrame::AlternativeLeft {
                    right,
                    input,
                    had_truthy,
                    out_cont,
                }) = self.frames.pop()
                else {
                    return Err(internal_contract("graph frame kind changed under pop"));
                };
                // The alternative's suppression region ends with its frame:
                // the fallback's own cell sites are NOT the miss the author
                // handled, so they fire normally.
                resources.exit_mismatch_suppression();
                if !had_truthy {
                    self.pending = Some(GraphTask::Eval {
                        node: right,
                        input,
                        cont: out_cont,
                    });
                }
                return Ok(true);
            }
            // A Collect at the top means the body is exhausted: the LIFO finish —
            // pop, and route the completed owned array through the out-continuation.
            Some(GraphFrame::Collect { .. }) => {
                // The completed array's register check must run unfrozen.
                self.path_exit_frozen();
                let Some(GraphFrame::Collect { array, out_cont }) = self.frames.pop() else {
                    return Err(internal_contract("graph frame kind changed under pop"));
                };
                self.pending = Some(GraphTask::Route {
                    value: EngineResult::owned(Value::Array(array)),
                    cont: out_cont,
                });
                return Ok(true);
            }
            // A CountCollect at the top means the body is exhausted: pop and
            // route the exact integer count (the array never existed). The
            // count fits `i64` for any array the process
            // can hold: each collected element costs at least one byte of
            // array storage, so 2^63 elements exceed every memory ceiling.
            Some(GraphFrame::CountCollect { .. }) => {
                // The completed count's register check must run unfrozen.
                self.path_exit_frozen();
                let Some(GraphFrame::CountCollect { count, out_cont }) = self.frames.pop() else {
                    return Err(internal_contract("graph frame kind changed under pop"));
                };
                let count = i64::try_from(count).unwrap_or(i64::MAX);
                let value =
                    Value::Number(Number::integer(jqf_data::Integer::from_i64(count)));
                self.pending = Some(GraphTask::Route {
                    value: EngineResult::owned(value),
                    cont: out_cont,
                });
                return Ok(true);
            }
            // An ObjectValue at the top means the member's value producer for this
            // key is exhausted: pop it, truncating the destination's partial back to
            // this member's depth so the next key/value starts clean.
            Some(GraphFrame::ObjectValue { .. }) => {
                let Some(GraphFrame::ObjectValue { dest, member, .. }) = self.frames.pop() else {
                    return Err(internal_contract("graph frame kind changed under pop"));
                };
                self.truncate_partial(dest, member)?;
                return Ok(true);
            }
            // A `ModifyFold` at the top drives its NEXT path, or — with the path
            // set exhausted — pops, applies the deferred deletions, and publishes
            // the one rewritten document.
            Some(GraphFrame::ModifyFold { .. }) => return self.advance_modify(resources),
            // A `KeyedCollect` at the top means one child's key filter is
            // exhausted: close that key and start the next child, or — with the
            // last child keyed — publish the mode's answer.
            Some(GraphFrame::KeyedCollect { .. }) => return self.advance_keyed(resources),
            // A `KeyedStreamKey` at the top means the element's key filter is
            // done: file the pending element into the accumulator and resume
            // the source generator.
            Some(GraphFrame::KeyedStreamKey { .. }) => return self.file_keyed_stream(resources),
            // A `KeyedStreamSource` at the top means the generator is exhausted:
            // pop and publish the aggregate once (the fused drive's only output,
            // byte-identical to the collect-then-key pair it replaced).
            Some(GraphFrame::KeyedStreamSource { .. }) => return self.finish_keyed_stream(resources),
            // A `Walk` at the top means the current child's walk is exhausted:
            // start the next child, or — with the last child rebuilt — pop and
            // run the rebuilt container through the mapped filter.
            Some(GraphFrame::Walk { .. }) => return self.advance_walk(resources),
            // A `MapValues` at the top means the current member's update is
            // exhausted: start the next member, or — with the last member
            // rebuilt — pop and publish the container directly (never through
            // the filter again).
            Some(GraphFrame::MapValues { .. }) => return self.advance_map_values(resources),
            // A `ModifyUpdate` at the top means the update generator is exhausted
            // for this path. If nothing was taken, the update produced NO output:
            // record the path on the fold's deferred deletion list (`|= empty`
            // deletes, and the deletion waits for the end of the fold).
            Some(GraphFrame::ModifyUpdate { .. }) => {
                let Some(GraphFrame::ModifyUpdate {
                    fold_frame,
                    path,
                    taken,
                    chain,
                }) = self.frames.pop()
                else {
                    return Err(internal_contract("graph frame kind changed under pop"));
                };
                if taken {
                    return Ok(true);
                }
                // The update emitted nothing, so the write never closed the
                // hole. Relink the vacated ancestors back into the fold so
                // the deferred deletion still sees the document; dropping
                // the chain would leave `null` in the accumulator.
                if let Some(chain) = chain {
                    self.restore_modify_chain(fold_frame, chain, resources)?;
                }
                match self.frames.as_mut_slice().get_mut(fold_frame) {
                    Some(GraphFrame::ModifyFold { deferred, mode, .. }) => {
                        // A `Set` update is the bound right-hand side, which
                        // always produces its one value — `= empty` never reaches
                        // the fold, because the binding kills it first.
                        if matches!(*mode, ModifyMode::Update) {
                            deferred
                                .try_reserve(1)
                                .map_err(|_| EngineRunError::allocation_failure())?;
                            deferred.push(path);
                        }
                    }
                    _ => return Err(internal_contract("modify fold frame kind changed under deferral"))
                }
                return Ok(true);
            }
            // A `FactAssignWrite` at the top means the update generator is
            // exhausted for this node: pop it and route the unchanged document.
            // If an output landed, the delta is already recorded; if not, the
            // update produced no payload and nothing is written (the recorded
            // `|= empty` narrowing — no delta, no deletion).
            Some(GraphFrame::FactAssignWrite { .. }) => {
                let Some(GraphFrame::FactAssignWrite {
                    document,
                    out_cont,
                    ..
                }) = self.frames.pop()
                else {
                    return Err(internal_contract("graph frame kind changed under fact write pop"));
                };
                // The popped document routes at `out_cont`, so THAT is the
                // position any raise downstream of this route must
                // enclosure-test against until the next refresh — the same
                // law [`Self::consume_answer`] applies before routing an
                // answer. Leaving it at the update generator's last routing
                // position would let a barrier enclosing this construction
                // mis-judge the failed value's continuation.
                self.raise_cont = out_cont;
                self.pending = Some(GraphTask::Route {
                    value: document,
                    cont: out_cont,
                });
                return Ok(true);
            }
            // An `IndexedEach` at the top walks its equal-key run, then pops —
            // the same top-of-stack law as `Each`, over a different cursor.
            Some(GraphFrame::CallFilter { .. }) => {
                let _ = self.pop_frame_releasing(resources);
                return Ok(true);
            }
            Some(GraphFrame::IndexedEach { .. }) => return self.advance_indexed_each(resources),
            // A generator at the top emits its next value, opens its next
            // step, or pops when it is spent — one match arm per row of
            // [`jqf_builtins::semantics::generate::GENERATORS`].
            Some(GraphFrame::Generate { .. }) => return self.advance_generate(resources),
            Some(GraphFrame::Each { .. }) => {}
        }
        let (child, emitted_cursor, stage, next_at, cont) = {
            let frame = self
                .frames
                .as_mut_slice()
                .last_mut()
                .ok_or_else(|| internal_contract("graph frame kind changed under advance"))?;
            let GraphFrame::Each {
                container,
                cursor,
                stage,
                next_at,
                cont,
            } = frame
            else {
                return Err(internal_contract("graph frame kind changed under advance"));
            };
            let child = {
                let mut scratch = StepScratch::new(&mut self.workspace, resources).with_deltas(&self.fact_deltas);
                next_child(container, *cursor, &mut scratch)?
            };
            let emitted = *cursor;
            *cursor += 1;
            (child, emitted, *stage, *next_at, *cont)
        };
        if let Some(child) = child {
            // The child's component extends the register, scoped by a
            // PathStep marker (the borrow is released first).
            if self.path.is_some() {
                let component = {
                    let frame = self
                        .frames
                        .as_slice()
                        .last()
                        .ok_or_else(|| internal_contract("graph frame kind changed under child"))?;
                    let GraphFrame::Each { container, .. } = frame else {
                        return Err(internal_contract("graph frame kind changed under child"));
                    };
                    path_register::child_component(container, emitted_cursor, resources)?
                };
                self.path_each_child(&child, component, cont, resources)?;
            }
            // A trailing `.[]` child's remaining residual is empty: the child
            // is already the leaf. A Descend task at that offset would return
            // the value untouched after one extra dispatch hop. Route it
            // instead — the indexed-each producer uses the same terminal-`.[]`
            // law. A residual still remaining (`.[] .a`, `..` children
            // resuming AT the descend step) keeps Descend.
            self.pending = Some(if each_child_is_leaf(self.nodes, stage, next_at) {
                GraphTask::Route { value: child, cont }
            } else {
                GraphTask::Descend {
                    node: stage,
                    value: child,
                    at: next_at,
                    cont,
                }
            });
        } else {
            // The container is exhausted: pop the frame and resume its parent.
            self.frames.pop();
        }
        Ok(true)
    }

    /// Pushes one continuation frame.
    pub(crate) fn push_frame(&mut self, frame: GraphFrame<'program, 'source>) {
        self.frames.push(frame);
    }

    /// Discards the entire frame stack and marks the run done.
    pub(crate) fn tear_down(&mut self, resources: &ResourceContext<'_>) {
        self.done = true;
        self.pending = None;
        // The wholesale discard bypasses `pop_frame_releasing`, so every live
        // `Try` / `AlternativeLeft` frame's suppression region must be exited
        // here or the depth leaks on the shared request context.
        let live_barriers = self
            .frames
            .iter()
            .filter(|frame| matches!(frame, GraphFrame::Try { .. } | GraphFrame::AlternativeLeft { .. }))
            .count();
        for _ in 0..live_barriers {
            resources.exit_mismatch_suppression();
        }
        self.frames.clear();
    }
}

/// True when a fan-out child's remaining residual is empty, so the child is
/// already the leaf. A `Descend` at that offset would return the value
/// untouched; the child is routed instead.
///
/// Fail-closed: a non-`Stage` node id keeps the Descend path, which raises
/// the existing contract error rather than inventing a leaf.
fn each_child_is_leaf(nodes: &[ProgramNode], stage: ProgramNodeId, next_at: usize) -> bool {
    match nodes.get(stage.index()) {
        Some(ProgramNode::Stage { steps, .. }) => next_at >= steps.len(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceLimits, WorkMeter};

    fn each_step() -> StageStep {
        StageStep::new(StepAccess::Each, false)
    }

    fn key_step(name: &str) -> StageStep {
        StageStep::new(StepAccess::Key(String::from(name)), false)
    }

    fn descend_step() -> StageStep {
        StageStep::new(StepAccess::Descend, false)
    }

    fn stage_id(index: usize) -> ProgramNodeId {
        ProgramNodeId::from_index(index).expect("node id")
    }

    fn stage(steps: Vec<StageStep>) -> ProgramNode {
        ProgramNode::Stage {
            start: StageStart::Current,
            steps,
        }
    }

    #[test]
    fn trailing_each_child_is_a_leaf() {
        let nodes = [stage(vec![each_step()])];
        assert!(
            each_child_is_leaf(&nodes, stage_id(0), 1),
            "`.[]` children resume past the only step"
        );
        assert!(
            !each_child_is_leaf(&nodes, stage_id(0), 0),
            "resuming AT the each step is not a leaf"
        );
    }

    #[test]
    fn postfix_after_each_is_not_a_leaf() {
        let nodes = [stage(vec![each_step(), key_step("a")])];
        assert!(
            !each_child_is_leaf(&nodes, stage_id(0), 1),
            "`.[] .a` still has the key step"
        );
        assert!(
            each_child_is_leaf(&nodes, stage_id(0), 2),
            "past the key step the child is the leaf"
        );
    }

    #[test]
    fn nested_each_first_child_is_not_a_leaf() {
        let nodes = [stage(vec![each_step(), each_step()])];
        assert!(
            !each_child_is_leaf(&nodes, stage_id(0), 1),
            "`.[][]` first fan-out still has the second each"
        );
        assert!(each_child_is_leaf(&nodes, stage_id(0), 2), "the inner each is terminal");
    }

    #[test]
    fn recursive_descend_child_is_not_a_leaf() {
        // `..` resumes children AT the descend step so the walk recurses.
        let nodes = [stage(vec![descend_step()])];
        assert!(
            !each_child_is_leaf(&nodes, stage_id(0), 0),
            "`..` children re-enter the descend step"
        );
    }

    #[test]
    fn a_non_stage_node_fails_closed() {
        let nodes = [ProgramNode::CollectArray { body: None }];
        assert!(
            !each_child_is_leaf(&nodes, stage_id(0), 1),
            "a non-stage id must not be treated as a trailing leaf"
        );
    }

    fn test_resources() -> ResourceContext<'static> {
        static CONTROL: ContinueControl = ContinueControl;
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
    }

    fn owned_int_array() -> EngineResult<'static> {
        let mut array = Array::try_new().expect("array");
        for text in ["10", "20"] {
            array
                .try_push(Value::Number(Number::try_json_literal(text).expect("literal")))
                .expect("push");
        }
        EngineResult::owned(Value::Array(array))
    }

    fn seed_each_machine(nodes: &[ProgramNode]) -> GraphMachine<'_, 'static> {
        GraphMachine::seed(nodes, stage_id(0), stage_id(0), 0, 0, &[], &[], &[], owned_int_array())
            .expect("owned test seed")
    }

    fn drive_until_each(machine: &mut GraphMachine<'_, '_>, resources: &mut ResourceContext<'_>) {
        let mut guard = 0;
        loop {
            guard += 1;
            assert!(guard < 16, "Each frame must appear during setup");
            if machine.pending.is_none() && matches!(machine.frames.last(), Some(GraphFrame::Each { .. })) {
                return;
            }
            let task = machine
                .pending
                .take()
                .expect("setup must keep a pending task until the Each frame is pushed");
            machine.step_task(task, resources).expect("setup step");
        }
    }

    #[test]
    fn trailing_each_child_parks_a_route_task() {
        let mut resources = test_resources();
        let nodes = [stage(vec![each_step()])];
        let mut machine = seed_each_machine(&nodes);
        drive_until_each(&mut machine, &mut resources);
        machine.advance_frame(&mut resources).expect("advance");
        assert!(
            matches!(machine.pending, Some(GraphTask::Route { .. })),
            "a trailing `.[]` child must skip the Descend hop"
        );
    }

    #[test]
    fn postfix_each_child_still_parks_a_descend_task() {
        let mut resources = test_resources();
        let nodes = [stage(vec![each_step(), key_step("a")])];
        let mut machine = seed_each_machine(&nodes);
        drive_until_each(&mut machine, &mut resources);
        machine.advance_frame(&mut resources).expect("advance");
        assert!(
            matches!(machine.pending, Some(GraphTask::Descend { at: 1, .. })),
            "`.[] .a` must still descend into the key step"
        );
    }
}
