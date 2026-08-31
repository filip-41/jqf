//! Correlated scan, anti-join, user indexes, and partial-sort evaluation.
//!
//! One job: warm and probe the join tables on the program, plus the bounded
//! heap for a recognized top-k row. Nested machines inherit tables by the
//! graph-machine seed law.

#[allow(clippy::wildcard_imports)]
use super::super::*;

impl<'source> GraphMachine<'_, 'source> {
    /// Which index slot `node` names, or `None` when the closed recognition
    /// table does not name it at all.
    ///
    /// The table is empty for almost every program, and the linear walk is over
    /// at most a handful of rows, so this is one length test on the common path.
    pub(crate) fn scan_slot(&self, node: ProgramNodeId) -> Option<usize> {
        if self.scans.is_empty() {
            return None;
        }
        self.scans.iter().position(|scan| scan.scan() == node)
    }

    /// Which anti-join slot the `select` call `node` names, or `None` when the
    /// closed recognition table does not name it at all. The slots sit AFTER
    /// the equi-join's in the shared index-slot vector.
    pub(crate) fn anti_scan_slot(&self, node: ProgramNodeId) -> Option<usize> {
        if self.anti.is_empty() {
            return None;
        }
        self.anti.iter().position(|scan| scan.select() == node)
    }

    /// Offers one recognized anti-join `select` to its index, taking the
    /// indexed route only when every run-time side condition holds.
    ///
    /// The declines are the equi-join's own, re-checked for the NEGATED form:
    /// the element is DROPPED when the index holds a key equal to
    /// the probe — the naive predicate `isempty($o[] | (cond and empty))` is
    /// then false — and routed to the selection's out-continuation when it
    /// does not (the naive predicate is true). The twelve semantic hazards
    /// of the indexed anti-join:
    ///
    /// - the BUILD is total and all-or-nothing (a key-path navigation that
    ///   would raise declines the whole route, and the decline happens before
    ///   any element of this scan was emitted — the equi-join's DOWNGRADE
    ///   position);
    /// - the probe is total and single-valued, evaluated once per scan element
    ///   where the naive form evaluates it once per inner child — a raise
    ///   declines and the naive predicate raises in its own position, after
    ///   the elements before this one were already emitted;
    /// - an EMPTY inner container makes the naive predicate true for EVERY
    ///   element WITHOUT evaluating the probe at all, so the indexed route
    ///   declines and the naive path emits everything (the empty-container negated form);
    /// - NaN keys decline the build (the NaN-key decline law), and a NaN PROBE can never
    ///   compare `Equal` to a NaN-free key under either law — the existence
    ///   probe answers "no match", which is exactly the naive `!=`'s answer.
    pub(crate) fn try_anti_join(
        &mut self,
        slot: usize,
        input: &EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, EngineRunError> {
        let Some(row) = self.anti.get(slot).copied() else {
            return Ok(false);
        };
        // Hazard: LOCATED containers only — the cache key is a node handle,
        // which an owned container does not have. The container is the BOUND
        // inner source (`$o[]`), read from the env slot through the row's
        // container stage.
        let join_slot = self
            .scans
            .len()
            .checked_add(slot)
            .ok_or_else(|| internal_contract("anti-join slot overflow"))?;
        let Some((cursor, end, _)) = self.equal_key_run_for(
            join_slot,
            row.container_stage(),
            row.key_stage(),
            row.probe(),
            input,
            resources,
        )?
        else {
            return Ok(false);
        };
        if cursor != end {
            // A match: the naive `isempty` is FALSE — the selection drops the
            // element, publishing nothing for it.
            JOIN_ENGAGEMENTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            return Ok(true);
        }
        // No match: the naive `isempty` is TRUE — the selection emits the
        // element, exactly as a truthy predicate output would.
        JOIN_ENGAGEMENTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let emission = input.try_clone().map_err(EngineRunError::Codec)?;
        self.route(emission, cont, resources)?;
        Ok(true)
    }

    /// Offers one recognized scan to its index, taking the indexed route only
    /// when every run-time side condition holds.
    ///
    /// The declines are the soundness argument's run-time half, and every one of
    /// them lands on the naive scan with ZERO bytes emitted for this scan — the
    /// same position `count.rs`'s DOWNGRADE law relies on, reached here for free
    /// because the source iteration is the scan's first act.
    pub(crate) fn try_indexed_scan(
        &mut self,
        slot: usize,
        input: &EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ScanRoute, EngineRunError> {
        let Some(scan) = self.scans.get(slot).copied() else {
            return Ok(ScanRoute::Naive);
        };
        // Hazard: LOCATED sources only. The cache key is a node handle, which an
        // owned container does not have.
        let Some((cursor, end, container)) =
            self.equal_key_run_for(slot, scan.source(), scan.key_stage(), scan.probe(), input, resources)?
        else {
            return Ok(ScanRoute::Naive);
        };
        // No match at all: the scan produces nothing, which is exactly what the
        // `FlatMapBody` above reads from an exhausted upstream.
        if cursor == end {
            JOIN_ENGAGEMENTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            return Ok(ScanRoute::Indexed);
        }
        self.push_frame(GraphFrame::IndexedEach {
            container,
            slot,
            cursor,
            end,
            cont,
        });
        JOIN_ENGAGEMENTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        Ok(ScanRoute::Indexed)
    }

    /// Locate, count, prepare, probe, and bisect one join slot's equal-key run.
    ///
    /// Declines (`None`) on every run-time hazard both routes share: a
    /// non-located container, a non-countable or empty container, a failed
    /// warm-up, a non-total probe, or a too-deep key. An empty container is
    /// the indexed scan's probe-position hazard and the anti-join's
    /// empty-inner "naive predicate is true for every element" hazard — both
    /// decline before a probe runs.
    fn equal_key_run_for(
        &mut self,
        join_slot: usize,
        container_stage: ProgramNodeId,
        key_stage: ProgramNodeId,
        probe: ProgramNodeId,
        input: &EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<(usize, usize, Container<'source>)>, EngineRunError> {
        let Some(located) = self.scan_container(container_stage, input, resources)? else {
            return Ok(None);
        };
        let handle = located.node();
        let container = Container::Located(located);
        let Some(len) = located_child_count(&container)? else {
            return Ok(None);
        };
        if len == 0 {
            return Ok(None);
        }
        if !self.prepare_join_slot(join_slot, key_stage, handle, &container, len, resources)? {
            return Ok(None);
        }
        if self.join_slot_serves_user(join_slot) {
            USER_INDEX_PROBES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        let Some(key) = self.probe_value(probe, resources) else {
            return Ok(None);
        };
        let cursor_and_end = {
            let index = self
                .slot_index(join_slot)
                .ok_or_else(|| internal_contract("join index vanished after preparation"))?;
            equal_key_run(index.entries.as_slice(), &key)
        };
        let Some((cursor, end)) = cursor_and_end else {
            return Ok(None);
        };
        Ok(Some((cursor, end, container)))
    }

    /// Which partial-sort row `node` names, or `None` when the closed
    /// recognition table does not name it at all.
    ///
    /// The table is empty for almost every program, and the linear walk is over
    /// at most a handful of rows, so this is one length test on the common path.
    pub(crate) fn topk_row(&self, node: ProgramNodeId) -> Option<PartialSort> {
        if self.topk.is_empty() {
            return None;
        }
        self.topk.iter().copied().find(|row| row.flat_map() == node)
    }

    /// The partial-sort row's run-time side condition: whether this input may
    /// take the bounded-heap path at all.
    ///
    /// A plain `sort` row takes it for EVERY input — `sort` rejects a non-array
    /// with the same not-sortable message `top_k` raises, so the partial sort's
    /// error path is byte-identical. A `sort_by` row takes it only over an
    /// ARRAY: `sort_by`'s object arm collects a key per value and fails with
    /// the TWO-ARRAYS rejection, which is not the not-sortable message `top_k`
    /// raises, so a non-array input falls back to the floor (the ordinary
    /// `sort_by` evaluation) — the join table's run-time decline, exercised
    /// before a single byte is emitted.
    pub(crate) fn take_partial_sort(&self, row: PartialSort, input: &EngineResult<'source>) -> bool {
        let ProgramNode::Call { overload, .. } = &self.nodes[row.sort_call().index()] else {
            return false;
        };
        if overload.get() != jqf_builtins::registry::builtins::id::SORT_BY {
            return true;
        }
        match input {
            EngineResult::Owned(value) => value.kind() == ValueKind::Array,
            // A view failure is a decline: the floor re-materializes the input
            // and raises whatever the decode produced.
            EngineResult::Located(located) => stage::located_view(located.product().document(), located.node())
                .is_ok_and(|view| view.kind().is_ok_and(|kind| kind == ValueKind::Array)),
        }
    }

    /// Drives one recognized partial-sort row: materialize the input, evaluate
    /// the `by` projection per element (the `sort_by` row only), and answer
    /// with the bounded-heap `top_k` law instead of the full sort plus slice.
    ///
    /// The drive is `eval_topk`'s own shape with the count fixed to the slice's
    /// authored bound: same not-sortable raise, same per-element projection
    /// order, same stable tie law, same single-item publication. The slice
    /// bound law is the count law (both ceil the same reading), so a fractional
    /// bound answers exactly what the slice answers.
    pub(crate) fn eval_partial_sort(
        &mut self,
        row: PartialSort,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        let (by_graph, direction) = match &self.nodes[row.sort_call().index()] {
            ProgramNode::Call { args, .. } => (args.first().copied(), row.direction()),
            _ => {
                return Err(internal_contract("partial sort dispatch over a non-call node"));
            }
        };
        let source_node = input.located().map(LocatedProduct::node);
        let root = self.materialize_capture(input, resources)?;
        let slots = self.path_slots(&root, source_node, resources)?;

        let array_len = match root.untagged() {
            Value::Array(array) => array.len(),
            other => {
                let operand = message::dump_trunc_owned(other)?;
                let text = message::not_sortable_message(other.kind(), &operand)?;
                return Err(jqf_builtins::semantics::path::raise(&text, resources));
            }
        };
        let count_value = crate::analysis::topk_count(self.nodes, row)
            .ok_or_else(|| internal_contract("partial sort row without a literal count"))?;

        // A bare static key graph (`sort_by(.x)`, `sort_by(.[0])`) is driven
        // per child with the keyed STATIC lane — no task machinery per element
        // — and the bounded heap is fed INLINE, so the projection costs the
        // floor's and the partial sort adds O(n log k) on top. A general key
        // graph takes the general per-element drive, and whole-value `sort`
        // rows the law's own loop; both reuse `top_k_law` unchanged. Either
        // way the row's consumer shapes the bounded-heap result before it is
        // routed.
        let result = match by_graph {
            Some(by_g) if Self::static_key_steps(self.nodes, by_g).is_some() => {
                self.eval_partial_sort_static(by_g, direction, &root, array_len, &count_value, resources)?
            }
            by_g => {
                let mut projection_keys: alloc::vec::Vec<alloc::vec::Vec<Value>> = Vec::new();
                if let Some(by_g) = by_g {
                    projection_keys
                        .try_reserve_exact(array_len)
                        .map_err(|_| EngineRunError::allocation_failure())?;
                    let nodes = self.nodes;
                    for position in 0..array_len {
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
                let law = direction;
                top_k_builtins::top_k_law(law, &root, &count_value, &projection_keys, resources)?
            }
        };
        let published = Self::shape_partial_result(row, result, resources)?;
        self.route(EngineResult::owned(published), cont, resources)
    }

    /// Shapes the bounded-heap result (an array of k elements) into the row's
    /// published value.
    ///
    /// - The slice rows publish the k-array as-is.
    /// - The REVERSED-sort row publishes the k LARGEST in the REVERSED sort's
    ///   order — the array REVERSED. The LARGEST heap keeps the same elements
    ///   `sort_by(f) | .[-k:]` would (later source indices among equal keys)
    ///   and emits them ascending; reversing that emission reproduces
    ///   `reverse` of the stable sort byte for byte: keys descending, ties in
    ///   REVERSED source order.
    /// - The index rows publish the single element — `sort | .[0]` over an
    ///   EMPTY array is `null`, not `[]`, exactly what the index step answers.
    pub(crate) fn shape_partial_result(
        row: PartialSort,
        result: Value,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Value, EngineRunError> {
        match row.consumer() {
            TopKConsumer::HeadSlice | TopKConsumer::TailSlice => Ok(result),
            TopKConsumer::ReversedHeadSlice => order_builtins::reverse(&result, resources),
            TopKConsumer::FirstIndex | TopKConsumer::LastIndex => {
                let Value::Array(array) = result.untagged() else {
                    return Err(internal_contract(
                        "index-consumer partial sort produced a non-array result",
                    ));
                };
                Ok(array.get(0usize).cloned().unwrap_or(Value::Null))
            }
        }
    }

    /// Drives one partial-sort row whose projection is a bare static key
    /// stage, with the bounded heap fed INLINE: one pass over the array,
    /// O(k) heap memory, no key vector, no spill. The per-child projection
    /// runs through the SAME `descend` lane the keyed collect's fast lane
    /// uses, so the projection cost matches the floor's `sort_by` exactly and
    /// the partial sort is a pure win over the full sort.
    ///
    /// The key is the projection's output, folded under `sort_by`'s law: a
    /// suppressed `?` mismatch contributes ZERO outputs, whose fold is `null`
    /// (`key_for_element`'s empty-output arm), exactly as the floor's keyed
    /// collect stores `KeyValue::Empty`.
    #[allow(
        clippy::too_many_arguments,
        reason = "one recognizer drive: the key graph, direction, subject, count, and ledger are each a distinct boundary"
    )]
    pub(crate) fn eval_partial_sort_static(
        &mut self,
        by_g: ProgramNodeId,
        direction: top_k_builtins::TopKDirection,
        root: &Value,
        array_len: usize,
        count_value: &Value,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Value, EngineRunError> {
        let steps = Self::static_key_steps(self.nodes, by_g)
            .ok_or_else(|| internal_contract("static partial-sort lane over a non-static key graph"))?;
        let Some(mut heap) = top_k_builtins::heap_for(count_value, array_len, direction)? else {
            // The count clamped to zero: an empty array (or a zero count). The
            // slice's answer over the empty array is the empty array; the
            // caller's consumer shaping turns that into `null` for an index row.
            let empty = Value::Array(Array::try_new().map_err(|_| EngineRunError::allocation_failure())?);
            return Ok(empty);
        };
        let container = Container::Owned(root.clone());
        for cursor in 0..array_len {
            let mut scratch = StepScratch::new(&mut self.workspace, resources).with_deltas(&self.fact_deltas);
            let child = next_child(&container, cursor, &mut scratch)?
                .ok_or_else(|| internal_contract("partial-sort cursor past the array's last child"))?;
            let env: &[EngineResult<'source>] = &[];
            let key = match descend(steps, 0, env, child, 0, &mut scratch)? {
                Descended::Leaf(leaf) => self.materialize_capture(leaf, resources)?,
                // The fold rule: zero outputs fold to `null` — the same answer
                // `key_for_element` gives an empty key list, and the same
                // `KeyValue::Empty` the floor's keyed collect stores.
                Descended::Skip => Value::Null,
                Descended::FanOut { .. } | Descended::Descend { .. } => {
                    return Err(internal_contract(
                        "a static key stage fanned out under the partial-sort lane",
                    ));
                }
            };
            heap.offer(key, cursor)?;
        }
        let Value::Array(elements) = root.untagged() else {
            return Err(internal_contract("partial-sort static lane over a non-array subject"));
        };
        heap.finish(elements, resources)
    }

    /// Brings `slot` to a state where a probe may run, reporting whether it got
    /// there.
    ///
    /// The build is deferred to the SECOND sight of a container, and that is the
    /// whole of it. A scan that never repeats (`$o |
    /// map(select(.a == "x"))` evaluated once) must not pay for an index it
    /// cannot amortize, and a scan whose container CHANGES every time (a source
    /// bound INSIDE the outer loop) never reaches a second sight at all — so the
    /// worst case the warm-up admits is one extra naive scan out of `k`.
    pub(crate) fn prepare_join_slot(
        &mut self,
        slot: usize,
        key_stage: ProgramNodeId,
        handle: NodeHandle,
        container: &Container<'source>,
        len: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, EngineRunError> {
        let slots = self
            .scans
            .len()
            .checked_add(self.anti.len())
            .ok_or_else(|| internal_contract("join slot count overflow"))?;
        if self.joins.len() <= slot {
            if self.joins.try_reserve(slots).is_err() {
                return Ok(false);
            }
            self.joins.resize_with(slots, JoinSlot::default);
        }
        // A USER-declared index covering this (container node, key path) wins
        // immediately. The user paid the build cost
        // explicitly, so even the FIRST sight of the container probes instead
        // of warming up — the declaration is exactly the "this field is
        // filtered repeatedly" contract the warm-up exists to discover.
        if let Some(user) = self.match_user_index(key_stage, handle) {
            let held = self
                .joins
                .get_mut(slot)
                .ok_or_else(|| internal_contract("join slot missing after sizing"))?;
            if held.container != Some(handle) || held.user != Some(user) {
                *held = JoinSlot {
                    container: Some(handle),
                    index: None,
                    user: Some(user),
                    poisoned: false,
                };
            }
            return Ok(true);
        }
        let held = self
            .joins
            .get_mut(slot)
            .ok_or_else(|| internal_contract("join slot missing after sizing"))?;
        if held.container != Some(handle) {
            *held = JoinSlot::cold(handle);
            return Ok(false);
        }
        if held.poisoned {
            return Ok(false);
        }
        // A slot serving a user-declared index never builds its own: the user
        // index is already in the store, and the lookup above re-adopts it on
        // every sight. (Reachable only transiently — the lookup returns first —
        // but the guard keeps the invariant local instead of assumed.)
        if held.user.is_some() {
            return Ok(true);
        }
        if held.index.is_some() {
            return Ok(true);
        }
        let built = self.build_join_index(key_stage, container, len, resources)?;
        let held = self
            .joins
            .get_mut(slot)
            .ok_or_else(|| internal_contract("join slot missing after build"))?;
        let Some(index) = built else {
            held.poisoned = true;
            return Ok(false);
        };
        held.index = Some(index);
        Ok(true)
    }

    /// `declare_index/2` — the user-declared reusable index.
    ///
    /// A TRANSPARENT acceleration declaration: build a sorted keyed multimap
    /// over the located container at CONTAINER, keyed on each child's KEY path,
    /// publish it into [`Self::user_indexes`], and route the INPUT through
    /// unchanged — declaring an index may change SPEED, never BYTES.
    ///
    /// Both arguments are PATTERN-MATCHED static paths (the `path(f)`
    /// precedent), never evaluated as filters, so the declaration cannot raise
    /// where the program without it would not. Every decline below is a NO-OP
    /// declaration — the input routes through unchanged and nothing is stored:
    /// a non-`Stage`/non-`Current` argument, a `?`/`Each`/`Descend` step, an
    /// OWNED (non-located) input, a container navigation that would raise, a
    /// non-container at the path, and a NaN key (the NaN-free law, inherited from
    /// [`Self::build_join_index`]).
    pub(crate) fn eval_index_declare(
        &mut self,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let ProgramNode::Call { args, .. } = &self.nodes[node.index()] else {
            return Err(internal_contract("declare_index dispatch over a non-call node"));
        };
        if args.len() != 2 {
            return Err(internal_contract("declare_index/2 call without exactly two arguments"));
        }
        let Some(key_steps) = user_key_steps(self.nodes, args[1]) else {
            return self.route(input, cont, resources);
        };
        let ProgramNode::Stage {
            start: StageStart::Current,
            steps: container_steps,
        } = &self.nodes[args[0].index()]
        else {
            return self.route(input, cont, resources);
        };
        if !container_steps
            .iter()
            .all(|step| !step.is_optional() && is_static_key_step(step))
        {
            return self.route(input, cont, resources);
        }
        // The index is keyed on a LOCATED container's node identity; an owned
        // input has none.
        let EngineResult::Located(located) = &input else {
            return self.route(input, cont, resources);
        };
        let entry = EngineResult::Located(located.try_clone().map_err(EngineRunError::Codec)?);
        let descended = {
            let env = self.env.as_slice();
            let mut scratch = StepScratch::new(&mut self.workspace, resources).with_deltas(&self.fact_deltas);
            descend(container_steps, 0, env, entry, 0, &mut scratch)
        };
        // A navigation that would RAISE declines: the declaration's promise is
        // byte-identity with `.`, so it must not surface an error the program
        // without it would not reach.
        let Ok(Descended::Leaf(EngineResult::Located(container))) = descended else {
            return self.route(input, cont, resources);
        };
        let handle = container.node();
        let container = Container::Located(container);
        let Some(len) = located_child_count(&container)? else {
            return self.route(input, cont, resources);
        };
        let Some(index) = self.build_join_index(args[1], &container, len, resources)? else {
            return self.route(input, cont, resources);
        };
        // The store is keyed by (container node, key path): re-declaring the
        // same pair replaces the earlier build; every other pair coexists.
        self.user_indexes
            .try_reserve(1)
            .map_err(|_| EngineRunError::allocation_failure())?;
        if let Some(existing) = self
            .user_indexes
            .iter_mut()
            .find(|entry| entry.container == handle && entry.key_steps == key_steps)
        {
            existing.index = index;
        } else {
            self.user_indexes.push(UserIndexSlot {
                container: handle,
                key_steps,
                index,
            });
        }
        self.route(input, cont, resources)
    }

    /// The user-declared index slot covering `(handle, key_stage)`, or `None`.
    ///
    /// The key path is read from the scan's `key_stage` in the same canonical
    /// form the declaration stored, so a scan over the same (container node,
    /// key path) — and only that pair — matches. A container whose node handle
    /// changed (an edit produced a new revision, or the container is owned) is
    /// a MISS: the declared index is dropped, never authoritative.
    pub(crate) fn match_user_index(&self, key_stage: ProgramNodeId, handle: NodeHandle) -> Option<usize> {
        if self.user_indexes.is_empty() {
            return None;
        }
        let key_steps = user_key_steps(self.nodes, key_stage)?;
        self.user_indexes
            .iter()
            .position(|entry| entry.container == handle && entry.key_steps == key_steps)
    }

    /// Whether one join slot is currently serving a USER-declared index (as
    /// opposed to its own warm-up build) — the engagement probe's gate.
    pub(crate) fn join_slot_serves_user(&self, slot: usize) -> bool {
        self.joins.get(slot).is_some_and(|held| held.user.is_some())
    }

    /// The index one join slot currently serves: the slot's own warm-up build,
    /// or the user-declared index the slot adopted.
    pub(crate) fn slot_index(&self, slot: usize) -> Option<&JoinIndex> {
        let held = self.joins.get(slot)?;
        if let Some(index) = held.index.as_ref() {
            return Some(index);
        }
        let user = held.user?;
        self.user_indexes.get(user).map(|entry| &entry.index)
    }

    /// The LOCATED container a recognized scan iterates, or `None` for every
    /// representation and navigation outcome the route declines.
    ///
    /// A `Current`-start source (the generalized source row) iterates its
    /// static prefix over the scan's OWN input: an empty prefix (the `map`
    /// spelling's inner `[.[] | select(…)]` source, and the bare root `.[]`)
    /// makes the input itself the container; a non-empty one (the split-form
    /// `$o | .users[] | select(…)`) descends the prefix to find it. A
    /// `Variable`-start source (`$o[] | select(…)`) navigates the bound slot
    /// through the prefix before the `.[]`. A raise on either path is DROPPED
    /// rather than reported, because the naive source re-walks the identical
    /// path and raises in its own position.
    pub(crate) fn scan_container(
        &mut self,
        source_stage: ProgramNodeId,
        input: &EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<LocatedProduct<'source>>, EngineRunError> {
        let nodes = self.nodes;
        let ProgramNode::Stage { start, steps } = &nodes[source_stage.index()] else {
            return Ok(None);
        };
        let Some((_, prefix)) = steps.split_last() else {
            return Ok(None);
        };
        match start {
            StageStart::Current => {
                let EngineResult::Located(located) = input else {
                    return Ok(None);
                };
                if prefix.is_empty() {
                    return Ok(Some(located.try_clone().map_err(EngineRunError::Codec)?));
                }
                Self::descend_located_prefix(
                    prefix,
                    located,
                    self.env.as_slice(),
                    &mut self.workspace,
                    &self.fact_deltas,
                    resources,
                )
            }
            StageStart::Variable(variable) => {
                let variable = *variable;
                let Some(EngineResult::Located(bound)) = self.env.as_slice().get(variable as usize) else {
                    return Ok(None);
                };
                let bound = bound.try_clone().map_err(EngineRunError::Codec)?;
                Self::descend_located_prefix(
                    prefix,
                    &bound,
                    self.env.as_slice(),
                    &mut self.workspace,
                    &self.fact_deltas,
                    resources,
                )
            }
            StageStart::Literal(_) => Ok(None),
        }
    }

    /// Scratch+descend a located prefix to a located leaf, or decline.
    fn descend_located_prefix(
        prefix: &[StageStep],
        located: &LocatedProduct<'source>,
        env: &[EngineResult<'source>],
        workspace: &mut Option<MaterializeWorkspace>,
        fact_deltas: &[FactDelta],
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<LocatedProduct<'source>>, EngineRunError> {
        let entry = EngineResult::Located(located.try_clone().map_err(EngineRunError::Codec)?);
        let mut scratch = StepScratch::new(workspace, resources).with_deltas(fact_deltas);
        match descend(prefix, 0, env, entry, 0, &mut scratch) {
            Ok(Descended::Leaf(EngineResult::Located(located))) => Ok(Some(located)),
            _ => Ok(None),
        }
    }

    /// Builds the sorted keyed multimap over `container`'s children, or reports
    /// a DECLINE.
    ///
    /// The build is ALL-OR-NOTHING and TOTAL: the key path is navigated over
    /// every child before any child is emitted, and any outcome but a single
    /// leaf — a type mismatch, a suppressed step, a refused ledger grant —
    /// abandons the whole index. That is what lets the emitted-prefix argument
    /// stand: at decline time this scan has published nothing, so the naive path
    /// reproduces the error in its position.
    ///
    /// Note what does NOT decline. The null precedence makes `{}|.a` and
    /// `null|.a` both `null`, so a missing key and an explicit null land in the
    /// same equal-key run — which is exactly how `null == $x` behaves in the
    /// naive predicate. Only a navigation that would RAISE (`3|.a`) declines.
    pub(crate) fn build_join_index(
        &mut self,
        key_stage: ProgramNodeId,
        container: &Container<'source>,
        len: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<JoinIndex>, EngineRunError> {
        let nodes = self.nodes;
        let ProgramNode::Stage { steps: key_steps, .. } = &nodes[key_stage.index()] else {
            return Ok(None);
        };
        let mut entries = Vec::new();
        if entries.try_reserve_exact(len).is_err() {
            return Ok(None);
        }
        for cursor in 0..len {
            let Ok(position) = u32::try_from(cursor) else {
                return Ok(None);
            };
            let Some(child) = ({
                let mut scratch = StepScratch::new(&mut self.workspace, resources).with_deltas(&self.fact_deltas);
                next_child(container, cursor, &mut scratch)?
            }) else {
                return Ok(None);
            };
            let descended = {
                let env = self.env.as_slice();
                let mut scratch = StepScratch::new(&mut self.workspace, resources).with_deltas(&self.fact_deltas);
                descend(key_steps, 0, env, child, 0, &mut scratch)
            };
            let Ok(Descended::Leaf(leaf)) = descended else {
                return Ok(None);
            };
            let Ok(key) = self.materialize_uncharged(leaf, resources) else {
                return Ok(None);
            };
            // A NaN key breaks the index's whole equivalence argument (see
            // `JoinIndex`): the ordered index would find it where `==` refuses
            // it. Decline to the naive scan, which asks the operator directly.
            if order::carries_nan(&key) != Ok(false) {
                return Ok(None);
            }
            entries.push(JoinEntry { key, position });
        }
        // STABLE, so each equal-key run keeps ascending original position and
        // walking it reproduces the naive scan's emission order exactly. A key
        // that compares TOO DEEP declines the index entirely rather than
        // ordering on a made-up `Equal`: the naive scan then raises from `==`
        // in its own position, with `==`'s own message.
        let mut too_deep = false;
        entries.as_mut_slice().sort_by(|left, right| {
            order::total_cmp(&left.key, &right.key).unwrap_or_else(|_| {
                too_deep = true;
                core::cmp::Ordering::Equal
            })
        });
        if too_deep {
            return Ok(None);
        }
        Ok(Some(JoinIndex { entries }))
    }

    /// Evaluates the element-INDEPENDENT side of the indexed equality once.
    ///
    /// Every decline here is a decline to the naive scan, never an error: the
    /// naive predicate evaluates the very same expression per child and will
    /// raise there if it must.
    pub(crate) fn probe_value(&mut self, probe: ProgramNodeId, resources: &mut ResourceContext<'_>) -> Option<Value> {
        self.sync_probe(probe, resources).ok()?
    }

    /// Evaluates one probe-grammar node synchronously to its single owned
    /// value, or declines the indexed route.
    ///
    /// The grammar is the join table's PROBE rows (K1/K2/K3): K1/K2 leaves plus
    /// `Binary`/`Logical`/`Conditional` composition over them. Every leaf and
    /// every composition family is single-output by construction, so `Ok(Some)`
    /// is exactly one value; `Ok(None)` is a DECLINE — a node outside the
    /// grammar, a raise, or a suppressed step — and the caller falls back to
    /// the naive scan, which raises in its own position. `Err` is a machine
    /// error (allocation, cancellation) and propagates.
    ///
    /// The composition reuses the engine's OWN operators — the same
    /// [`jqf_builtins::semantics::binary::apply`] the `Binary` frames run, and the
    /// same truth law the `Logical` frames run — so a composed probe evaluates
    /// to the same value the naive per-child evaluation would, one scan per
    /// child replaced by one evaluation per scan.
    pub(crate) fn sync_probe(
        &mut self,
        node: ProgramNodeId,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<Value>, EngineRunError> {
        let nodes = self.nodes;
        match &nodes[node.index()] {
            ProgramNode::Stage { start, steps } => match start {
                // Row K2: a literal reads nothing at all.
                StageStart::Literal(literal) if steps.is_empty() => Ok(Some(literal.clone())),
                // Row K1: static steps over a bound slot, in whichever
                // representation the binder wrote.
                StageStart::Variable(variable) => {
                    let variable = *variable;
                    let descended = {
                        let env = self.env.as_slice();
                        let mut scratch =
                            StepScratch::new(&mut self.workspace, resources).with_deltas(&self.fact_deltas);
                        match env.get(variable as usize) {
                            Some(EngineResult::Located(located)) => match located.try_clone() {
                                Ok(handle) => descend(steps, 0, env, EngineResult::Located(handle), 0, &mut scratch),
                                Err(error) => Err(EngineRunError::Codec(error)),
                            },
                            Some(EngineResult::Owned(root)) => descend_borrowed(steps, 0, env, root, 0, &mut scratch),
                            None => return Ok(None),
                        }
                    };
                    let Ok(Descended::Leaf(leaf)) = descended else {
                        return Ok(None);
                    };
                    let Ok(value) = self.materialize_uncharged(leaf, resources) else {
                        return Ok(None);
                    };
                    Ok(Some(value))
                }
                // A `Current`-start stage reads the CURRENT ELEMENT — the key
                // path's side of the equality. Never a probe.
                StageStart::Current | StageStart::Literal(_) => Ok(None),
            },
            // Row K3, `Binary`: both operands must be probes and both must
            // yield their single value; the operator then applies exactly as
            // the `Binary` frame would, and a raise declines so the naive scan
            // raises in its own position.
            ProgramNode::Binary { op, left, right, .. } => {
                let Some(left) = self.sync_probe(*left, resources)? else {
                    return Ok(None);
                };
                let Some(right) = self.sync_probe(*right, resources)? else {
                    return Ok(None);
                };
                // The join probe evaluates the PROGRAM comparison node, so
                // the cross-kind ordering cell fires here exactly as at the
                // `BinaryLeft` frame.
                mismatch::note_cross_kind_ordering(*op, &left, &right, resources)?;
                Ok(jqf_builtins::semantics::binary::apply(*op, &left, &right, resources).ok())
            }
            // Row K3, `Logical`: the LEFT-outer short-circuiting law, copied
            // from the `LogicalLeft`/`LogicalRight` frames verbatim: a
            // short-circuit emits the boolean of the left's truthiness, and a
            // non-short-circuit booleanizes the right's truthiness.
            ProgramNode::Logical { operator, left, right } => {
                let Some(left) = self.sync_probe(*left, resources)? else {
                    return Ok(None);
                };
                let left = EngineResult::owned(left);
                let truthy = truth::is_truthy(&left).map_err(|_| internal_contract("probe truth check failed"))?;
                let short_circuits = match operator {
                    LogicalOp::And => !truthy,
                    LogicalOp::Or => truthy,
                };
                if short_circuits {
                    return Ok(Some(Value::Bool(truthy)));
                }
                let Some(right) = self.sync_probe(*right, resources)? else {
                    return Ok(None);
                };
                let right = EngineResult::owned(right);
                let truthy = truth::is_truthy(&right).map_err(|_| internal_contract("probe truth check failed"))?;
                Ok(Some(Value::Bool(truthy)))
            }
            // Row K3, `Conditional`: the condition selects its arm per output,
            // and the chosen arm must itself be a single-value probe.
            ProgramNode::Conditional {
                condition,
                consequent,
                alternative,
            } => {
                let Some(condition) = self.sync_probe(*condition, resources)? else {
                    return Ok(None);
                };
                let condition = EngineResult::owned(condition);
                let truthy = truth::is_truthy(&condition).map_err(|_| internal_contract("probe truth check failed"))?;
                self.sync_probe(if truthy { *consequent } else { *alternative }, resources)
            }
            // Everything else — a `Call`, `Try`, `Choice`, a constructor, a
            // binder — is not a row of the probe grammar.
            _ => Ok(None),
        }
    }

    /// Emits the next child of an [`GraphFrame::IndexedEach`] run, or pops the
    /// exhausted frame.
    ///
    /// The child is ROUTED directly rather than descended: both source rows
    /// require the `.[]` to be its stage's last step, so a `Descend` task at the
    /// step past the iteration would return the value untouched.
    pub(crate) fn advance_indexed_each(&mut self, resources: &mut ResourceContext<'_>) -> Result<bool, EngineRunError> {
        let Some(GraphFrame::IndexedEach {
            slot,
            cursor,
            end,
            cont,
            ..
        }) = self.frames.as_slice().last()
        else {
            return Err(internal_contract("graph frame kind changed under advance"));
        };
        let (slot, cursor, end, cont) = (*slot, *cursor, *end, *cont);
        if cursor >= end {
            self.frames.pop();
            return Ok(true);
        }
        let position = self
            .slot_index(slot)
            .and_then(|index| index.entries.as_slice().get(cursor))
            .map(|entry| entry.position as usize)
            .ok_or_else(|| internal_contract("indexed fan-out lost its index"))?;
        let Some(GraphFrame::IndexedEach { container, cursor, .. }) = self.frames.as_mut_slice().last_mut() else {
            return Err(internal_contract("graph frame kind changed under advance"));
        };
        *cursor += 1;
        let child = {
            let mut scratch = StepScratch::new(&mut self.workspace, resources).with_deltas(&self.fact_deltas);
            next_child(container, position, &mut scratch)?
        }
        .ok_or_else(|| internal_contract("indexed position outside its own container"))?;
        self.pending = Some(GraphTask::Route { value: child, cont });
        Ok(true)
    }
}
