//! Keyed aggregates: per-child keys, static-key fast path, streamed keyed folds.
//!
//! One job: collect keys and finish a keyed builtin. A bare Key/Index key
//! graph skips the task machinery. Call dispatch lives in [`super`].

#[allow(clippy::wildcard_imports)]
use super::super::*;

impl<'program, 'source> GraphMachine<'program, 'source> {
    /// Collects per-child keys for a keyed builtin.
    ///
    /// The two things that happen HERE and not at the end are the order. A
    /// non-ITERABLE input is rejected before the key filter runs at all
    /// (`null | sort_by(error("x"))` is `Cannot iterate over null (null)`,
    /// never `"x"`), because `map([f])` iterates first. An input that
    /// iterates but is not an ARRAY is NOT rejected here: an object drives the
    /// whole key collection and only then fails the both-arrays test, with the
    /// collected keys in the message.
    pub(crate) fn eval_keyed(
        &mut self,
        mode: jqf_builtins::semantics::keyed::KeyMode,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let key_graph = match &self.nodes[node.index()] {
            ProgramNode::Call { args, .. } => *args
                .first()
                .ok_or_else(|| internal_contract("a keyed builtin call with no argument"))?,
            _ => return Err(internal_contract("keyed dispatch over a non-call node")),
        };
        // The static fast lane: a bare `Key`/`Index` stage key graph runs per
        // child without the task machinery (borrowed located children, no
        // subject materialization until publication).
        if let Some(steps) = Self::static_key_steps(self.nodes, key_graph) {
            crate::exec::KEYED_STATIC_ENGAGEMENTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            return self.eval_keyed_static(mode, steps, input, cont, resources);
        }
        self.raise_cont = cont;
        let subject = self.materialize_capture(input, resources)?;
        let width = match subject.untagged() {
            Value::Array(array) => array.len(),
            Value::Object(object) => object.len(),
            other => {
                let operand = message::dump_trunc_owned(other)?;
                let text = message::iterate_message(other.kind(), &operand)?;
                return Err(jqf_builtins::semantics::path::raise(&text, resources));
            }
        };
        if width == 0 {
            let value = finish_keyed(mode, &subject, &mut Vec::new(), &[], resources)?;
            let result = self.retain_owned(value);
            return self.route(result, cont, resources);
        }
        let child = keyed_child(&subject, 0)?;
        self.push_frame(GraphFrame::KeyedCollect {
            mode,
            subject,
            width,
            cursor: 0,
            key: jqf_builtins::semantics::keyed::KeyValue::Empty,
            entries: Vec::new(),
            spill_runs: Vec::new(),
            spill_declined: false,
            spill_key_bytes: 0,
            key_graph,
            out_cont: cont,
        });
        self.pending = Some(GraphTask::Eval {
            node: key_graph,
            input: EngineResult::owned(child),
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Consumes one key-filter output into the current child's key.
    ///
    /// The common case — exactly one output — stores the value DIRECTLY,
    /// unboxed, so a keyed collect of 200k single-output keys pays no `Array`
    /// allocation per row. The box is only materialized when a SECOND output
    /// arrives, which is also when the entries Vec starts holding any.
    pub(crate) fn consume_keyed(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        let owned = self.materialize_capture(value, resources)?;
        let Some(GraphFrame::KeyedCollect {
            key, spill_key_bytes, ..
        }) = self.frames.as_mut_slice().get_mut(index)
        else {
            return Err(internal_contract("keyed frame kind changed under capture"));
        };
        // The spill budget's KEY estimate: an upper bound on the key's
        // encoded run bytes, accumulated per key output — the input half of
        // [`Self::keyed_spill_meter`]'s trigger.
        *spill_key_bytes = spill_key_bytes.saturating_add(Self::encoded_estimate(&owned));
        let previous = core::mem::replace(key, jqf_builtins::semantics::keyed::KeyValue::Empty);
        *key = Self::grow_key(previous, owned)?;
        Ok(())
    }

    /// An upper bound on one key value's encoded spill bytes: strings carry
    /// their length, containers carry their element count (the `_by` key is an
    /// ARRAY of the key filter's outputs, so the common case is a small array
    /// of scalars and the bound is tight), and the scalar payloads are the
    /// tags plus the canonical text's own length. How the bound feeds the
    /// flush trigger is [`Self::keyed_spill_meter`]'s law.
    pub(crate) fn encoded_estimate(value: &Value) -> usize {
        if value.is_temporal() {
            return 32;
        }
        match value.untagged() {
            Value::Null | Value::Bool(_) => 2,
            Value::String(text) => 6 + text.len(),
            Value::Number(_) => 24,
            Value::Array(array) => 8 + array.len().saturating_mul(16),
            Value::Object(object) => 8 + object.len().saturating_mul(24),
            Value::Bytes(bytes) => 6 + bytes.len(),
            Value::Tagged { payload, .. } => Self::encoded_estimate(payload),
            // The temporal pre-guard above handled every temporal kind. Any
            // kind ADDED later lands here, and a coarse floor keeps the
            // trigger heuristic total: an estimate that is off only loosens
            // or tightens the memory bound, never the output.
            _ => 32,
        }
    }

    /// The spill trigger's per-entry residency floor, in metered bytes.
    ///
    /// The key estimate alone under-meters a keyed collection: one numeric
    /// key estimates 24 bytes while the entry that holds it (`KeyValue` +
    /// position + `Vec` slack) costs several times that, so a small-key sort
    /// over many records crossed no budget and never spilled. Each pushed
    /// entry adds this floor to the metered total — a LOWER bound on the
    /// entry's real residency (32-byte `Value` slot plus discriminant,
    /// position, amortized growth), never an upper claim.
    const KEYED_SPILL_PER_ENTRY_FLOOR_BYTES: usize = 64;

    /// The metered spill total for a collection: the running encoded-key
    /// estimate plus one residency floor per buffered entry. Only decides
    /// WHEN the flush fires; the flush encodes the real bytes.
    fn keyed_spill_meter(spill_key_bytes: usize, entries: usize) -> usize {
        spill_key_bytes.saturating_add(entries.saturating_mul(Self::KEYED_SPILL_PER_ENTRY_FLOOR_BYTES))
    }

    /// Spill flush for keyed SORT-family collection when the metered total
    /// ([`Self::keyed_spill_meter`]) crosses the budget. Clears entries on
    /// success, marks declined on refusal.
    fn keyed_sort_spill_flush(
        resources: &mut ResourceContext<'_>,
        entries: &mut Vec<jqf_builtins::semantics::keyed::KeyedEntry>,
        spill_runs: &mut Vec<jqf_resource::RunId>,
        spill_declined: &mut bool,
        spill_key_bytes: &mut usize,
    ) -> Result<(), EngineRunError> {
        if *spill_declined
            || Self::keyed_spill_meter(*spill_key_bytes, entries.len())
                < usize::try_from(resources.limits().max_spill_bytes()).unwrap_or(usize::MAX)
            || resources.limits().max_spill_bytes() == 0
        {
            return Ok(());
        }
        match jqf_builtins::semantics::spill::try_flush(
            resources
                .spill_store()
                .ok_or_else(|| EngineRunError::internal_contract("spill store vanished"))?,
            entries,
            spill_runs,
            resources,
        ) {
            Ok(true) => {
                entries.clear();
                *spill_key_bytes = 0;
            }
            Ok(false) => {
                // The closed table declined (or the store was absent):
                // sticky — the whole collection stays in memory.
                *spill_declined = true;
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }

    /// The keyed SORT spill's TAIL: the last chunk under the budget never hit
    /// [`Self::keyed_sort_spill_flush`]'s predicate, so it must flush here or
    /// the merge would silently drop it. A DECLINE is not a contract break —
    /// earlier flushes only prove THEIR chunks were scalar; the closed table
    /// declines any chunk holding a non-scalar key. Recovery folds the runs'
    /// rows back into the in-memory table and clears the runs, so publication
    /// goes through the ordinary sort with zero observable difference from a
    /// never-spilled collection. A vanished store still fails the request —
    /// half-spilled is not the floor.
    fn keyed_spill_tail(
        resources: &mut ResourceContext<'_>,
        mode: jqf_builtins::semantics::keyed::KeyMode,
        entries: &mut Vec<jqf_builtins::semantics::keyed::KeyedEntry>,
        spill_runs: &mut Vec<jqf_resource::RunId>,
        store_contract: &'static str,
    ) -> Result<(), EngineRunError> {
        if mode != jqf_builtins::semantics::keyed::KeyMode::Sort || spill_runs.is_empty() || entries.is_empty() {
            return Ok(());
        }
        let store = resources
            .spill_store()
            .ok_or_else(|| internal_contract(store_contract))?;
        if jqf_builtins::semantics::spill::try_flush(store, entries, spill_runs, resources)? {
            entries.clear();
            return Ok(());
        }
        let recovered = jqf_builtins::semantics::spill::read_run_entries(store, spill_runs)?;
        entries
            .try_reserve(recovered.len())
            .map_err(|_| EngineRunError::allocation_failure())?;
        entries.extend(recovered);
        spill_runs.clear();
        Ok(())
    }

    /// Closes the current child's key and starts the next child, or publishes.
    pub(crate) fn advance_keyed(&mut self, resources: &mut ResourceContext<'_>) -> Result<bool, EngineRunError> {
        let top = self.frames.as_slice().len() - 1;
        let (started, key_graph, child) = {
            let Some(GraphFrame::KeyedCollect {
                mode,
                subject,
                width,
                cursor,
                key,
                entries,
                spill_runs,
                spill_declined,
                spill_key_bytes,
                key_graph,
                ..
            }) = self.frames.as_mut_slice().get_mut(top)
            else {
                return Err(internal_contract("keyed frame kind changed under advance"));
            };
            entries
                .try_reserve(1)
                .map_err(|_| EngineRunError::allocation_failure())?;
            entries.push(jqf_builtins::semantics::keyed::KeyedEntry {
                key: core::mem::replace(key, jqf_builtins::semantics::keyed::KeyValue::Empty),
                position: *cursor,
            });
            // The spill flush: SORT-family modes only —
            // their publication consumes the merged positions. group_by and
            // unique_by run-detection needs the adjacent ORDER, which the
            // merge does not expose in v1, so they stay in memory (the
            // documented follow-on). When the running key bytes exceed the
            // budget and the spill has not declined, write the entries so far
            // as one sorted run and clear them. The check runs per CHILD
            // (once per entry push), so the flush never re-enters.
            if *mode == jqf_builtins::semantics::keyed::KeyMode::Sort {
                Self::keyed_sort_spill_flush(resources, entries, spill_runs, spill_declined, spill_key_bytes)?;
            }
            *cursor += 1;
            if *cursor >= *width {
                (false, *key_graph, Value::Null)
            } else {
                (true, *key_graph, keyed_child(subject, *cursor)?)
            }
        };
        if started {
            self.pending = Some(GraphTask::Eval {
                node: key_graph,
                input: EngineResult::owned(child),
                cont: self.frames.len(),
            });
            return Ok(true);
        }
        let Some(GraphFrame::KeyedCollect {
            mode,
            subject,
            mut entries,
            spill_runs,
            out_cont,
            ..
        }) = self.frames.pop()
        else {
            return Err(internal_contract("graph frame kind changed under pop"));
        };
        self.raise_cont = out_cont;
        let mut spill_runs = spill_runs;
        Self::keyed_spill_tail(
            resources,
            mode,
            &mut entries,
            &mut spill_runs,
            "spill store vanished at finish",
        )?;
        let value = finish_keyed(mode, &subject, &mut entries, &spill_runs, resources)?;
        let result = self.retain_owned(value);
        self.pending = Some(GraphTask::Route {
            value: result,
            cont: out_cont,
        });
        Ok(true)
    }

    /// The streaming-aggregate recognition table: a pipe whose upstream is a
    /// literal collect (`[gen]`) and whose sole body is a keyed GROUP or
    /// SELECTION call of exactly one filter argument. `Sort`/`Unique` decline —
    /// their publication consumes the sorted whole collection (and the spill
    /// merge), which this drive never builds.
    ///
    /// Purely a shape read over the arena; the compiled graph is unchanged, so
    /// every analysis/classification answer above this line is unchanged too.
    pub(crate) fn streamed_aggregate(
        nodes: &[ProgramNode],
        node: ProgramNodeId,
    ) -> Option<(ProgramNodeId, jqf_builtins::semantics::keyed::KeyMode, ProgramNodeId)> {
        use jqf_builtins::semantics::keyed::KeyMode;
        let ProgramNode::FlatMap { upstream, body } = &nodes[node.index()] else {
            return None;
        };
        let ProgramNode::CollectArray { body: Some(generator) } = &nodes[upstream.index()] else {
            return None;
        };
        let ProgramNode::Call {
            payload: Evaluator::Keyed(mode @ (KeyMode::Group | KeyMode::Min | KeyMode::Max)),
            args,
            ..
        } = &nodes[body.index()]
        else {
            return None;
        };
        let [key_graph] = args.as_slice() else {
            return None;
        };
        Some((*generator, *mode, *key_graph))
    }

    /// Opens the streaming aggregate drive for a fused `[gen] | keyed(f)` pipe:
    /// the generator runs ONCE into this frame's accumulator and each output is
    /// filed as it arrives, so the collect array never exists. The published
    /// value routes once, through the same continuation the unfused pair uses.
    #[allow(clippy::unnecessary_wraps, reason = "the eval arms' uniform Option-returning shape")]
    pub(crate) fn eval_keyed_stream(
        &mut self,
        mode: jqf_builtins::semantics::keyed::KeyMode,
        generator: ProgramNodeId,
        key_graph: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
    ) -> Option<EngineResult<'source>> {
        // The collect this drive replaces was a frozen subexpression position;
        // the fused barrier keeps that law for its whole interior (generator
        // AND key graphs).
        self.path_enter_frozen();
        self.raise_cont = cont;
        self.push_frame(GraphFrame::KeyedStreamSource {
            mode,
            key_graph,
            groups: jqf_builtins::semantics::keyed::KeyedGroups::new(),
            winner: None,
            out_cont: cont,
        });
        self.pending = Some(GraphTask::Eval {
            node: generator,
            input,
            cont: self.frames.len(),
        });
        None
    }

    /// Consumes one generated element ([`GraphMachine::route`]'s walk): hold it
    /// pending and start its key filter over a clone. Nothing publishes here —
    /// like [`Self::consume_keyed_source`], only more so.
    pub(crate) fn consume_keyed_stream_source(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        let key_graph = match self.frames.as_slice().get(index) {
            Some(GraphFrame::KeyedStreamSource { key_graph, .. }) => *key_graph,
            _ => {
                return Err(internal_contract(
                    "keyed stream source frame kind changed under capture",
                ));
            }
        };
        let owned = self.materialize_capture(value, resources)?;
        let keys_input = owned.clone();
        self.push_frame(GraphFrame::KeyedStreamKey {
            source_frame: index,
            pending: Some(owned),
            key: jqf_builtins::semantics::keyed::KeyValue::Empty,
        });
        self.pending = Some(GraphTask::Eval {
            node: key_graph,
            input: EngineResult::owned(keys_input),
            cont: self.frames.len(),
        });
        Ok(())
    }

    /// Captures one key output for the pending element —
    /// [`Self::consume_keyed`]'s boxing law minus the spill accounting the
    /// streamed drive never enters (spill is SORT-family only).
    pub(crate) fn consume_keyed_stream_key(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        let owned = self.materialize_capture(value, resources)?;
        let Some(GraphFrame::KeyedStreamKey { key, .. }) = self.frames.as_mut_slice().get_mut(index) else {
            return Err(internal_contract("keyed stream key frame kind changed under capture"));
        };
        let previous = core::mem::replace(key, jqf_builtins::semantics::keyed::KeyValue::Empty);
        *key = Self::grow_key(previous, owned)?;
        Ok(())
    }

    /// Grows one child's captured key by one more output — `Empty → One →
    /// Many`, the unboxed single-output law both keyed drives share.
    fn grow_key(
        previous: jqf_builtins::semantics::keyed::KeyValue,
        output: Value,
    ) -> Result<jqf_builtins::semantics::keyed::KeyValue, EngineRunError> {
        use jqf_builtins::semantics::keyed::KeyValue;
        match previous {
            KeyValue::Empty => Ok(KeyValue::One(output)),
            KeyValue::One(first) => {
                let mut array = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
                array
                    .try_push(first)
                    .map_err(|_| EngineRunError::allocation_failure())?;
                array
                    .try_push(output)
                    .map_err(|_| EngineRunError::allocation_failure())?;
                Ok(KeyValue::Many(array))
            }
            KeyValue::Many(mut array) => {
                array
                    .try_push(output)
                    .map_err(|_| EngineRunError::allocation_failure())?;
                Ok(KeyValue::Many(array))
            }
            KeyValue::Bare(_) => Err(internal_contract("a bare key inside a keyed per-child capture")),
        }
    }

    /// Files the finished element into the source frame's accumulator
    /// ([`advance_frame`]'s top-of-stack arm): the mode's ONE law per element,
    /// then resume the source generator.
    pub(crate) fn file_keyed_stream(&mut self, resources: &mut ResourceContext<'_>) -> Result<bool, EngineRunError> {
        use jqf_builtins::semantics::keyed::{KeyMode, selection_replaces};
        let Some(GraphFrame::KeyedStreamKey {
            source_frame,
            pending,
            key,
        }) = self.frames.pop()
        else {
            return Err(internal_contract("graph frame kind changed under keyed-stream file"));
        };
        let element = pending.ok_or_else(|| internal_contract("a keyed stream element vanished"))?;
        let Some(GraphFrame::KeyedStreamSource {
            mode,
            groups,
            winner,
            out_cont,
            ..
        }) = self.frames.as_mut_slice().get_mut(source_frame)
        else {
            return Err(internal_contract("keyed stream source frame kind changed under file"));
        };
        // A filing comparison that runs too deep raises from the source's own
        // continuation ([`advance_keyed`]'s law for its publish-side raise).
        self.raise_cont = *out_cont;
        match mode {
            KeyMode::Group => groups.file(key, element, resources)?,
            KeyMode::Min | KeyMode::Max => {
                let larger_wins = *mode == KeyMode::Max;
                let replace = match winner {
                    None => true,
                    Some((incumbent_key, _)) => selection_replaces(larger_wins, &key, incumbent_key)
                        .map_err(|_| jqf_builtins::semantics::depth::comparison_error(resources))?,
                };
                if replace {
                    *winner = Some((key, element));
                }
            }
            // Declined at recognition; never lowered onto this drive.
            KeyMode::Sort | KeyMode::Unique => {
                return Err(internal_contract(
                    "the streaming aggregate drive filed for a sort-family mode",
                ));
            }
        }
        Ok(true)
    }

    /// Publishes the exhausted aggregate ([`advance_frame`]'s top-of-stack
    /// arm): ONE value, byte-identical to [`finish_keyed`] over the same
    /// elements — the groups in published order, or the selection winner, or
    /// the mode's empty law (`[]` / `null`).
    pub(crate) fn finish_keyed_stream(&mut self, _resources: &mut ResourceContext<'_>) -> Result<bool, EngineRunError> {
        use jqf_builtins::semantics::keyed::KeyMode;
        // The completed aggregate's register check must run unfrozen.
        self.path_exit_frozen();
        let Some(GraphFrame::KeyedStreamSource {
            mode,
            groups,
            winner,
            out_cont,
            ..
        }) = self.frames.pop()
        else {
            return Err(internal_contract("graph frame kind changed under keyed-stream finish"));
        };
        // The publication's allocation failures raise at the aggregate's own
        // continuation, exactly as [`advance_keyed`]'s publish-side raises do.
        self.raise_cont = out_cont;
        let value = match mode {
            KeyMode::Group => groups.publish()?,
            KeyMode::Min | KeyMode::Max => winner.map_or(Value::Null, |(_, element)| element),
            KeyMode::Sort | KeyMode::Unique => {
                return Err(internal_contract(
                    "the streaming aggregate drive finished for a sort-family mode",
                ));
            }
        };
        self.pending = Some(GraphTask::Route {
            value: EngineResult::owned(value),
            cont: out_cont,
        });
        Ok(true)
    }

    /// Whether a keyed call's key graph is a BARE STATIC STAGE — a
    /// `StageStart::Current` stage of only `Key`/`Index` steps, so exactly one
    /// output per child with no fan-out and no environment.
    pub(crate) fn static_key_steps(nodes: &[ProgramNode], key: ProgramNodeId) -> Option<&[StageStep]> {
        let ProgramNode::Stage {
            start: StageStart::Current,
            steps,
        } = &nodes[key.index()]
        else {
            return None;
        };
        steps
            .iter()
            .all(|step| matches!(step.access(), StepAccess::Key(_) | StepAccess::Index(_)))
            .then_some(steps)
    }

    /// The keyed fast lane: a key graph that is a bare static stage is driven
    /// per child WITHOUT the task machinery — the child is a BORROWED located
    /// view when the input is located (no subject materialization, no per-child
    /// clone), the stage steps run directly, and the entry collection, spill
    /// accounting and publication are [`Self::advance_keyed`]'s own, so the
    /// published bytes and laws are the general drive's.
    ///
    /// The lane is entered only for located inputs with the static shape; an
    /// owned input or any other key graph takes the general drive, whose
    /// behavior this lane reproduces for the shape it owns.
    #[allow(
        clippy::too_many_lines,
        reason = "one drive mirroring the general keyed frame's entry, per-child and publish                   laws in one place, so the two drives cannot drift apart"
    )]
    pub(crate) fn eval_keyed_static(
        &mut self,
        mode: jqf_builtins::semantics::keyed::KeyMode,
        steps: &'program [StageStep],
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        use jqf_builtins::semantics::keyed::KeyedEntry;
        self.raise_cont = cont;
        // The container the children come from: located children stay borrowed;
        // an owned container clones children out (the general drive's cost).
        let container = match &input {
            EngineResult::Located(located) => Container::Located(located.try_clone().map_err(EngineRunError::Codec)?),
            EngineResult::Owned(value) => match value.untagged() {
                Value::Array(_) | Value::Object(_) => Container::Owned(value.clone()),
                other => {
                    let operand = message::dump_trunc_owned(other)?;
                    let text = message::iterate_message(other.kind(), &operand)?;
                    return Err(jqf_builtins::semantics::path::raise(&text, resources));
                }
            },
        };
        let width = match &container {
            Container::Located(_) => {
                if let Some(width) = located_child_count(&container)? {
                    width
                } else {
                    // A located NON-container: the general drive materializes
                    // before it rejects, so reject with the same message here.
                    let subject = self.materialize_capture(input, resources)?;
                    let operand = message::dump_trunc_owned(subject.untagged())?;
                    let text = message::iterate_message(subject.kind(), &operand)?;
                    return Err(jqf_builtins::semantics::path::raise(&text, resources));
                }
            }
            Container::Owned(value) => match value.untagged() {
                Value::Array(array) => array.len(),
                Value::Object(object) => object.len(),
                _ => unreachable!("owned container validated above"),
            },
            Container::LocatedFiltered(..) => {
                unreachable!("filtered markup containers never reach the keyed drive")
            }
        };
        if width == 0 {
            let subject = self.materialize_capture(input, resources)?;
            let value = finish_keyed(mode, &subject, &mut Vec::new(), &[], resources)?;
            let result = self.retain_owned(value);
            return self.route(result, cont, resources);
        }
        let mut entries: Vec<KeyedEntry> = Vec::new();
        let mut spill_runs: Vec<jqf_resource::RunId> = Vec::new();
        let mut spill_declined = false;
        let mut spill_key_bytes = 0usize;
        for cursor in 0..width {
            let mut scratch = StepScratch::new(&mut self.workspace, resources).with_deltas(&self.fact_deltas);
            let child = next_child(&container, cursor, &mut scratch)?
                .ok_or_else(|| internal_contract("keyed cursor past the subject's last child"))?;
            let descended = {
                let env: &[EngineResult<'source>] = &[];
                descend(steps, 0, env, child, 0, &mut scratch)
            }?;
            let key = match descended {
                // A suppressed mismatch (`Key?`/`Index?`) contributes NO key
                // output: the general drive stores the child's EMPTY key array.
                Descended::Skip => jqf_builtins::semantics::keyed::KeyValue::Empty,
                // The static shape emits exactly one leaf per child, stored
                // UNBOXED: the one-output `[value]` box is carried virtually
                // in the compare at depth + 1 (see [`KeyValue`]).
                Descended::Leaf(leaf) => {
                    let owned = self.materialize_capture(leaf, resources)?;
                    // The spill budget's trigger estimate, [`Self::consume_keyed`]'s
                    // own: without this accumulation the static lane's flush check
                    // below could never fire (the budget silently did nothing for
                    // the static key shapes).
                    spill_key_bytes = spill_key_bytes.saturating_add(Self::encoded_estimate(&owned));
                    jqf_builtins::semantics::keyed::KeyValue::One(owned)
                }
                Descended::FanOut { .. } | Descended::Descend { .. } => {
                    return Err(internal_contract(
                        "a static key stage fanned out under the keyed fast lane",
                    ));
                }
            };
            entries
                .try_reserve(1)
                .map_err(|_| EngineRunError::allocation_failure())?;
            entries.push(KeyedEntry { key, position: cursor });
            // The spill flush, [`Self::advance_keyed`]'s own: SORT-family only,
            // once per entry push, on the shared meter (keys + entry floors).
            if mode == jqf_builtins::semantics::keyed::KeyMode::Sort {
                Self::keyed_sort_spill_flush(
                    resources,
                    &mut entries,
                    &mut spill_runs,
                    &mut spill_declined,
                    &mut spill_key_bytes,
                )?;
            }
        }
        Self::keyed_spill_tail(
            resources,
            mode,
            &mut entries,
            &mut spill_runs,
            "spill store vanished at keyed static finish",
        )?;
        // The subject is materialized only here, for publication: the keyed
        // fast lane never materializes the whole container per child.
        let subject = self.materialize_capture(input, resources)?;
        let value = finish_keyed(mode, &subject, &mut entries, &spill_runs, resources)?;
        let result = self.retain_owned(value);
        self.pending = Some(GraphTask::Route { value: result, cont });
        Ok(None)
    }
}
