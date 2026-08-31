//! Assignment and fact-write drives.
//!
//! One job: `Modify` and `FactAssign` — path collection, update fold, and
//! restore. Select is a call evaluator and lives in [`super::call`].

#[allow(clippy::wildcard_imports)]
use super::super::*;

impl<'source> GraphMachine<'_, 'source> {
    /// Drives one FACT assignment: resolve the value path to its located node
    /// in the original document, then run the update over the node's current
    /// fact payload (or the bound value, for `=`), recording the resulting
    /// delta for the edit lane. The document itself is never mutated — the
    /// node's payload becomes a [`FactDelta`] the SDK applies as a span
    /// operation under `--edit` — and the pipe input is emitted unchanged once the update
    /// completes (an owned result after a preceding value write passes through).
    /// Query-time reads overlay the recorded deltas so a later accessor sees
    /// the write without mutating the document.
    ///
    /// A value path that resolves to NO node (a missing key or an out-of-range
    /// index) writes nothing and passes the document through: the fact write
    /// has no value to create (recorded narrowing — `=` would create the value
    /// path, and facts have no owned value to fabricate).
    #[allow(
        clippy::too_many_arguments,
        reason = "one edit-lane drive keeps its node ids, role, kind, mode, input, and routing explicit"
    )]
    pub(crate) fn eval_fact_assign(
        &mut self,
        paths: ProgramNodeId,
        role: &str,
        kind: &str,
        selector_slot: Option<(VarSlot, bool)>,
        update: ProgramNodeId,
        mode: ModifyMode,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        // A dynamic selector (`PATH.@(expr)` / `PATH.&(expr)`) resolves its
        // bound name FIRST, against the ORIGINAL input the lowering bound it
        // with — an unknown role is a typed rejection even when the value path
        // would miss (a rejection is never silenced by a no-op path).
        let (role, kind) = match selector_slot {
            Some((slot, attribute)) => resolve_fact_write_selector(&self.env, slot, attribute, resources)?,
            None => (role.to_owned(), kind.to_owned()),
        };
        // A preceding value write emits Owned. The span authority is the seed
        // document: fact deltas name original nodes, never the rebuilt value.
        let located = match &input {
            EngineResult::Located(located) => located,
            EngineResult::Owned(_) => self
                .origin
                .as_ref()
                .ok_or_else(|| internal_contract("fact assignment over a non-located input"))?,
        };
        let document = located.product().document();
        let target = walk_fact_path(self.nodes, paths, document, located.node(), &self.env, resources)?;
        let Some((path, node)) = target else {
            self.pending = Some(GraphTask::Route { value: input, cont });
            return Ok(None);
        };
        let seed = if matches!(mode, ModifyMode::Update) {
            // `|=` sees the CURRENT payload as its dot: an existing comment
            // list, or `null` for a node with none (`null + ["x"]` is
            // `["x"]`, so an append-on-first-write is an ordinary update).
            // The attribute role reads by KIND (`.&href |= …` seeds the
            // existing `href` attribute); the comment roles by role alone.
            let handle = document
                .node_handle(node)
                .map_err(|_| internal_contract("fact path node handle"))?;
            let mut scratch = StepScratch::new(&mut self.workspace, resources).with_deltas(&self.fact_deltas);
            let attribute = !kind.is_empty();
            let selector: &str = if attribute { &kind } else { &role };
            scratch.read_attached_fact(document, handle, selector, attribute)?
        } else {
            Value::Null
        };
        let document_out = input.try_clone().map_err(EngineRunError::Codec)?;
        self.push_frame(GraphFrame::FactAssignWrite {
            node,
            path,
            role,
            kind,
            taken: false,
            document: document_out,
            out_cont: cont,
        });
        self.pending = Some(GraphTask::Eval {
            node: update,
            input: EngineResult::owned(seed),
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Consumes one `FactAssign` UPDATE output: the FIRST output becomes the
    /// node's new payload (recorded as a delta); later outputs are discarded —
    /// the lowering already caps an `Update` graph at one output, exactly as
    /// it caps a `Modify` update, and this guard keeps the law independent of
    /// the lowering.
    pub(crate) fn consume_fact_assign(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let (node, path, role, kind) = match self.frames.as_slice().get(index) {
            Some(GraphFrame::FactAssignWrite {
                node, path, role, kind, ..
            }) => (*node, path.clone(), role.clone(), kind.clone()),
            _ => {
                return Err(internal_contract("graph frame kind changed under fact write"));
            }
        };
        let payload = match value {
            EngineResult::Owned(owned) => owned,
            EngineResult::Located(located) => {
                let workspace = self.workspace.get_or_insert_with(jqf_data::MaterializeWorkspace::new);
                located
                    .product()
                    .document()
                    .materialize_node_with(workspace, located.node(), resources)
                    .map_err(|_| internal_contract("fact payload materialization"))?
            }
        };
        // The comment-fact canonical payload is a LIST of text LINES (the read
        // side round-trips `["x"]` as `# x` back to `["x"]`), so a lone string
        // — or a list item carrying newlines — is coerced to the list it
        // denotes, splitting on line boundaries. A multi-line string must
        // SPLIT rather than render its first line only: the edit lane's verify
        // re-decode compares read-back lines to the recorded payload, and a
        // truncated first line could never round-trip. The normalization is
        // ROLE-keyed: the comment roles (the line-based positions) split on
        // line boundaries, while an attribute payload (`.&href = "2"`) is a
        // single text value that must pass through untouched — a line split
        // would corrupt it. Any other payload shape stays untouched and is
        // refused by the splice's own contract message.
        let payload = GraphMachine::normalize_fact_payload(&payload, &role)?;
        match self.frames.as_mut_slice().get_mut(index) {
            Some(GraphFrame::FactAssignWrite { taken, .. }) => *taken = true,
            _ => {
                return Err(internal_contract("graph frame kind changed under fact write"));
            }
        }
        self.fact_deltas
            .try_reserve(1)
            .map_err(|_| EngineRunError::allocation_failure())?;
        self.fact_deltas.push(FactDelta {
            path,
            node,
            role,
            kind,
            payload,
        });
        Ok(None)
    }

    /// The comment-fact canonical payload is a LIST of text LINES (see
    /// [`GraphMachine::consume_fact_assign`]), so every string — a lone
    /// payload or a list item — is split on line boundaries into the list
    /// form. Non-string shapes pass through untouched: the edit lane's splice
    /// refuses them with a message naming the contract.
    ///
    /// The split applies to the COMMENT roles only (the line-based positions
    /// the read side round-trips as lists); an ATTRIBUTE payload is a single
    /// text value and passes through unchanged.
    pub(crate) fn normalize_fact_payload(payload: &Value, role: &str) -> Result<Value, EngineRunError> {
        if !is_comment_role(role) {
            return Ok(payload.clone());
        }
        match payload.untagged() {
            Value::String(text) => {
                let mut lines = Vec::new();
                for line in text.as_str().split(['\n', '\r']) {
                    lines.push(Value::try_string(line).map_err(|_| EngineRunError::allocation_failure())?);
                }
                Ok(Value::Array(
                    jqf_data::Array::try_from_vec(lines).map_err(|_| EngineRunError::allocation_failure())?,
                ))
            }
            Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    if let Value::String(text) = item.untagged() {
                        for line in text.as_str().split(['\n', '\r']) {
                            out.push(Value::try_string(line).map_err(|_| EngineRunError::allocation_failure())?);
                        }
                    } else {
                        out.push(item.clone());
                    }
                }
                Ok(Value::Array(
                    jqf_data::Array::try_from_vec(out).map_err(|_| EngineRunError::allocation_failure())?,
                ))
            }
            other => Ok(other.clone()),
        }
    }

    /// Drives one assignment: name every path against the ORIGINAL input
    /// (`_modify` folds over `path(paths)`, so the generator never sees the
    /// fold's edits), then open the fold that rewrites them one at a time.
    pub(crate) fn eval_modify(
        &mut self,
        paths: ProgramNodeId,
        update: ProgramNodeId,
        mode: ModifyMode,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.raise_cont = cont;
        // The path expression runs on the SAME machine; the assignment
        // DISCARDS the prefix. The root is materialized FRESH (the
        // assignment's output is a computed value — even an empty path set's
        // unchanged document must fail the register's check).
        let body_input = input.try_clone().map_err(EngineRunError::Codec)?;
        let source_node = input.located().map(LocatedProduct::node);
        let root = self.materialize_capture(input, resources)?;
        let saved = self.path.take();
        self.path = Some(path_register::PathRegister::new(root.clone(), source_node));
        self.push_frame(GraphFrame::PathPublish {
            saved,
            values: Vec::new(),
            pending: None,
            out_cont: cont,
            finish: PathFinish::Assignment,
            assign: Some(AssignmentTarget {
                update,
                mode,
                root: root.clone(),
                out_cont: cont,
            }),
        });
        self.pending = Some(GraphTask::Eval {
            node: paths,
            input: body_input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Completes an assignment's path collection into the fold.
    pub(crate) fn finish_assignment_paths(
        &mut self,
        named: Vec<Value>,
        pending: Option<EngineRunError>,
        assign: Option<AssignmentTarget>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        if let Some(failure) = pending {
            return Err(failure);
        }
        let Some(AssignmentTarget {
            update,
            mode,
            root,
            out_cont,
        }) = assign
        else {
            return Err(internal_contract("assignment completion without an assignment target"));
        };
        let may_vacate = jqf_builtins::semantics::path::later_extensions(&named, resources)?
            .into_iter()
            .map(|has_extension| !has_extension)
            .collect::<Vec<_>>();
        let state = self.hold_state(EngineResult::owned(root), resources)?;
        self.push_frame(GraphFrame::ModifyFold {
            paths: named,
            may_vacate,
            cursor: 0,
            state,
            deferred: Vec::new(),
            update,
            mode,
            out_cont,
        });
        Ok(())
    }

    /// Advances one assignment fold by one path, or finishes it.
    ///
    /// `Set`'s update is the BOUND right-hand side (reads no input); `Update`'s
    /// runs with dot = the path's value IN THE ACCUMULATOR — the law that makes
    /// an outer edit visible to an inner one (`.. |= (if type=="object" then 1
    /// else . end)` RAISES, because the root's flip is what the next `getpath`
    /// walks into).
    pub(crate) fn advance_modify(&mut self, resources: &mut ResourceContext<'_>) -> Result<bool, EngineRunError> {
        let index = self
            .frames
            .as_slice()
            .len()
            .checked_sub(1)
            .ok_or_else(|| internal_contract("modify advance with no frame"))?;
        let step = match self.frames.as_mut_slice().get_mut(index) {
            Some(GraphFrame::ModifyFold {
                paths,
                may_vacate,
                cursor,
                update,
                mode,
                out_cont,
                ..
            }) => match paths.get(*cursor) {
                None => None,
                Some(path) => {
                    let path = path.clone();
                    let may_vacate = may_vacate
                        .get(*cursor)
                        .copied()
                        .ok_or_else(|| internal_contract("modify fold vacate row missing"))?;
                    *cursor += 1;
                    Some((path, may_vacate, *update, *mode, *out_cont))
                }
            },
            _ => return Err(internal_contract("graph frame kind changed under advance")),
        };
        let Some((path, may_vacate, update, mode, out_cont)) = step else {
            return self.finish_modify(index, resources);
        };
        let (seed, chain) = match mode {
            ModifyMode::Set => (Value::Null, None),
            ModifyMode::Update => self.vacate_modify_seed(index, &path, may_vacate, out_cont, resources)?,
        };
        self.push_frame(GraphFrame::ModifyUpdate {
            fold_frame: index,
            path,
            taken: false,
            chain,
        });
        self.pending = Some(GraphTask::Eval {
            node: update,
            input: EngineResult::owned(seed),
            cont: self.frames.len(),
        });
        Ok(true)
    }

    /// Reads one `Update` step's dot out of the accumulator, VACATING it where
    /// that is unobservable.
    ///
    /// The update's whole job is to produce the value that replaces what it read,
    /// so handing it a second handle on the accumulator's copy is handing it a
    /// SHARED operand: `.a += $x` then finds its left operand non-unique, and
    /// [`binary::apply_owned`]'s in-place extension degrades to the rebuild it
    /// exists to avoid — O(container) per step, O(n²) over the fold.
    /// [`semantics_path::take_path_chain`] leaves `null` in the slot instead, so
    /// the value arrives as the last handle on its allocation and `+` grows it.
    /// The matching write refills the recorded slots and does not walk the
    /// path again.
    ///
    /// The accumulator is MOVED out of its cell. A successful vacate keeps the
    /// ancestor containers on the chain the `ModifyUpdate` frame carries — they
    /// are not relinked into one `Value`, because that tree would only be
    /// walked again. The hole is closed by [`Self::consume_modify_update`]; an
    /// update that emits nothing relinks the chain back so the deferred
    /// deletion still sees the document. A path the vacating walk declines is
    /// read the old way through [`semantics_path::get_path`] and written
    /// through [`semantics_path::set_path`]. The update graph reads its dot,
    /// never the accumulator.
    pub(crate) fn vacate_modify_seed(
        &mut self,
        index: usize,
        path: &Value,
        may_vacate: bool,
        out_cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(Value, Option<semantics_path::VacatedChain>), EngineRunError> {
        let document = match self.frames.as_mut_slice().get_mut(index) {
            Some(GraphFrame::ModifyFold { state, .. }) => state.take(),
            _ => {
                return Err(internal_contract("modify fold frame kind changed under read"));
            }
        };
        // A path a LATER path descends through is read intact: the vacate's
        // hole would answer `null` to the descendant's walk where the value is
        // the answer. The in-place win is bounded to the paths that
        // can actually keep their hole — leaves and single-path sets — which
        // is exactly where the growing-accumulator shape lives.
        let (seed, document, chain) = if may_vacate {
            match semantics_path::take_path_chain(document, path, resources) {
                Ok(semantics_path::VacateResult::Taken { value, chain }) => (Ok(value), None, Some(chain)),
                Ok(semantics_path::VacateResult::Declined(document)) => (
                    semantics_path::get_path(&document, path, resources),
                    Some(document),
                    None,
                ),
                Err(error) => {
                    self.raise_cont = out_cont;
                    return Err(error);
                }
            }
        } else {
            (
                semantics_path::get_path(&document, path, resources),
                Some(document),
                None,
            )
        };
        // A kept chain owns the ancestor containers. Re-holding would relink
        // them into one tree the write would then discard. `take` already left
        // `null` in the fold cell; that placeholder stays until the write or
        // the empty-update restore.
        if let Some(document) = document {
            let cell = self.hold_state(EngineResult::owned(document), resources)?;
            match self.frames.as_mut_slice().get_mut(index) {
                Some(GraphFrame::ModifyFold { state, .. }) => *state = cell,
                _ => {
                    return Err(internal_contract("modify fold frame kind changed under rehold"));
                }
            }
        }
        match seed {
            Ok(value) => Ok((value, chain)),
            Err(error) => {
                self.raise_cont = out_cont;
                Err(error)
            }
        }
    }

    /// Completes one assignment fold: apply the deferred deletions as ONE
    /// `delpaths`, then publish the single rewritten document.
    pub(crate) fn finish_modify(
        &mut self,
        index: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, EngineRunError> {
        if index + 1 != self.frames.len() {
            return Err(internal_contract("modify fold finished below the stack top"));
        }
        let Some(GraphFrame::ModifyFold {
            mut state,
            deferred,
            out_cont,
            ..
        }) = self.frames.pop()
        else {
            return Err(internal_contract("graph frame kind changed under pop"));
        };
        self.raise_cont = out_cont;
        let document = state.take();
        let value = if deferred.is_empty() {
            document
        } else {
            let set = Array::try_from_vec(deferred)
                .map(Value::Array)
                .map_err(|_| EngineRunError::allocation_failure())?;
            jqf_builtins::semantics::pathset::delete_paths(&document, &set, resources)?
        };
        // The assignment's output is a COMPUTED value: inside a path body it
        // always fails the register's check (`path(.a = 1)` is `with result
        // {"a":1}`), even when an empty path set left the document
        // byte-identical.
        if self.path.is_some() {
            return Err(jqf_builtins::semantics::path::invalid_result(&value, resources));
        }
        self.pending = Some(GraphTask::Route {
            value: EngineResult::owned(value),
            cont: out_cont,
        });
        Ok(true)
    }

    /// Consumes one assignment UPDATE output: it is written into the enclosing
    /// fold's accumulator at the step's path.
    ///
    /// Only the FIRST output lands. The lowering already caps an `Update` graph
    /// at one output with the `label`/`break` shape — which is what makes
    /// `.a |= (1, error("x"))` produce `{"a":1}` rather than raise — so the guard
    /// here is what keeps that law from depending on the lowering alone.
    pub(crate) fn consume_modify_update(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let (fold_frame, path, taken, chain) = match self.frames.as_mut_slice().get_mut(index) {
            Some(GraphFrame::ModifyUpdate {
                fold_frame,
                path,
                taken,
                chain,
            }) => (*fold_frame, path.clone(), *taken, chain.take()),
            _ => {
                return Err(internal_contract("modify update frame kind changed under capture"));
            }
        };
        if taken {
            return Ok(None);
        }
        let new = self.materialize_capture(value, resources)?;
        // The accumulator is TAKEN out of the frame, not borrowed: `set_path`
        // writes through a container it owns alone, which turns the per-path
        // rebuild of every ancestor into a slot write. A vacated step writes
        // the recorded chain instead — same slot write, no second walk.
        // Taking also releases the cell's residency here, and the rewritten
        // document is re-held below — the same take-and-rehold law the reduce
        // state already runs on. A raise between the two leaves `null` behind,
        // which is exactly what an aborted fold publishes: nothing.
        let (written, out_cont) = match self.frames.as_mut_slice().get_mut(fold_frame) {
            Some(GraphFrame::ModifyFold { state, out_cont, .. }) => {
                let out_cont = *out_cont;
                let document = state.take();
                let written = match chain {
                    Some(chain) => chain.write(new, resources),
                    None => semantics_path::set_path(document, &path, new, resources),
                };
                (written, out_cont)
            }
            _ => {
                return Err(internal_contract("modify fold frame kind changed under update"));
            }
        };
        let written = match written {
            Ok(written) => written,
            Err(error) => {
                self.raise_cont = out_cont;
                return Err(error);
            }
        };
        let cell = self.hold_state(EngineResult::owned(written), resources)?;
        match self.frames.as_mut_slice().get_mut(index) {
            Some(GraphFrame::ModifyUpdate { taken, .. }) => *taken = true,
            _ => {
                return Err(internal_contract("modify update frame kind changed under write"));
            }
        }
        match self.frames.as_mut_slice().get_mut(fold_frame) {
            Some(GraphFrame::ModifyFold { state, .. }) => {
                *state = cell;
                Ok(None)
            }
            _ => Err(internal_contract("modify fold frame kind changed under write")),
        }
    }

    /// Relinks a vacated chain back into the fold's accumulator.
    ///
    /// An update that emitted nothing never closed the hole. The leaf slot
    /// already holds `null`, so writing `null` is the restore: the deferred
    /// deletion then sees the same holed document the take-then-set ABI would
    /// have left. A raise here publishes nothing.
    pub(crate) fn restore_modify_chain(
        &mut self,
        fold_frame: usize,
        chain: semantics_path::VacatedChain,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        let out_cont = match self.frames.as_slice().get(fold_frame) {
            Some(GraphFrame::ModifyFold { out_cont, .. }) => *out_cont,
            _ => {
                return Err(internal_contract("modify fold frame kind changed under restore"));
            }
        };
        let restored = match chain.write(Value::Null, resources) {
            Ok(restored) => restored,
            Err(error) => {
                self.raise_cont = out_cont;
                return Err(error);
            }
        };
        let cell = self.hold_state(EngineResult::owned(restored), resources)?;
        match self.frames.as_mut_slice().get_mut(fold_frame) {
            Some(GraphFrame::ModifyFold { state, .. }) => {
                *state = cell;
                Ok(())
            }
            _ => Err(internal_contract("modify fold frame kind changed under restore")),
        }
    }

    /// Materializes every bound binder slot for a path-mode run.
    ///
    /// A slot bound to the SAME document node as the run's input is aliased onto
    /// the already-materialized root rather than materialized a second time. That
    /// is not an optimization: the identity test is what makes `. as $x |
    /// path($x.a)` a path at all, and two independent copies of one document
    /// node are not identical.
    pub(crate) fn path_slots(
        &mut self,
        root: &Value,
        source_node: Option<NodeHandle>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Vec<Option<Value>>, EngineRunError> {
        // The snapshot must cover the WHOLE program slot namespace, not the
        // env's current length: the env is grown lazily, so a path family
        // (`|=`, `path`, `setpath`, a callable's argument) that runs before
        // any binder forced the full size would hand the path-mode machine a
        // short slot vector, and a callable's absolute `param_slots`
        // would land out of range. Unpopulated slots are pre-filled with null
        // exactly as the graph machine's `ensure_env` pre-fills its env.
        let bound = usize::try_from(self.slots).map_err(|_| EngineRunError::allocation_failure())?;
        let mut slots = core::mem::take(&mut self.slot_scratch);
        slots.clear();
        slots
            .try_reserve_exact(bound)
            .map_err(|_| EngineRunError::allocation_failure())?;
        for index in 0..bound {
            let Some(result) = self.env.as_slice().get(index) else {
                slots.push(Some(Value::Null));
                continue;
            };
            let held = result.try_clone().map_err(EngineRunError::Codec)?;
            let aliased = matches!(
                (held.located().map(LocatedProduct::node), source_node),
                (Some(held_node), Some(source)) if held_node == source
            );
            let value = if aliased {
                root.clone()
            } else {
                self.materialize_capture(held, resources)?
            };
            slots.push(Some(value));
        }
        Ok(slots)
    }

    /// Returns a path-slots overlay vec to the reusable scratch.
    pub(crate) fn recycle_slot_scratch(&mut self, mut slots: Vec<Option<Value>>) {
        slots.clear();
        if slots.capacity() > self.slot_scratch.capacity() {
            self.slot_scratch = slots;
        }
    }
}
