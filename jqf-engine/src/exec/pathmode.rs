//! Path-family handoff from the graph machine into the path register.
//!
//! One job: evaluate `path`/`paths`/`getpath`/`setpath`/`delpaths` and the
//! assignment-mode publish frames. The register itself lives in
//! [`super::path_register`]; this file is the machine's call into it.

#[allow(clippy::wildcard_imports)]
use super::*;

impl<'source> GraphMachine<'_, 'source> {
    /// One body output: a tracked value collects the register's path (Law 3).
    pub(crate) fn consume_path_publish(
        &mut self,
        index: usize,
        value: &EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        let Some(register) = self.path.as_ref() else {
            return Err(internal_contract("path publish consumed a value with no register live"));
        };
        if !register.tracks_result(value) {
            let owned = self.path_owned(value, resources)?;
            return Err(jqf_builtins::semantics::path::invalid_result(&owned, resources));
        }
        let path = register.path_value(resources)?;
        let GraphFrame::PathPublish { values, .. } = &mut self.frames.as_mut_slice()[index] else {
            return Err(internal_contract("path publish frame kind changed under capture"));
        };
        values
            .try_reserve(1)
            .map_err(|_| EngineRunError::allocation_failure())?;
        values.push(path);
        Ok(())
    }

    /// Drives one path-family builtin over the single machine: the tracked
    /// families share the walker drive, the reads answer once per argument
    /// output, and the writes run their modify fold.
    pub(crate) fn eval_path_family(
        &mut self,
        family: path_register::PathFamily,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        match family {
            // The tracked families share the walker drive (the shape gate
            // decides whether the body streams or takes the register run);
            // the reads answer once per argument output.
            path_register::PathFamily::Path | path_register::PathFamily::Paths => {
                self.eval_path_walk(family, node, input, cont, resources)
            }
            path_register::PathFamily::GetPath | path_register::PathFamily::DelPaths => {
                self.eval_path_answer(family, node, input, cont, resources)
            }
            path_register::PathFamily::SetPath => self.eval_set_path(node, input, cont, resources),
        }
    }

    /// The call node's argument ids, copied (the frame drive keeps the nodes
    /// borrowed across the drive).
    pub(crate) fn call_arg_ids(
        &self,
        node: ProgramNodeId,
        what: &'static str,
    ) -> Result<Vec<ProgramNodeId>, EngineRunError> {
        let ProgramNode::Call { args, .. } = &self.nodes[node.index()] else {
            return Err(internal_contract(what));
        };
        let mut copied = Vec::new();
        copied
            .try_reserve_exact(args.len())
            .map_err(|_| EngineRunError::allocation_failure())?;
        copied.extend_from_slice(args);
        Ok(copied)
    }

    /// Opens one path read call's argument-answering drive: retain the
    /// materialized subject, then run the argument filter into the shared
    /// [`GraphFrame::ArgumentAnswer`] frame that answers per output.
    ///
    /// The C builtins answer once per argument output, so `getpath([] , error)`
    /// publishes the answer for `[]` before the argument's second output
    /// raises. The shared frame routes an answer-computation failure and the
    /// argument generator's own failure from the call's out-continuation with
    /// the published prefix standing.
    pub(crate) fn eval_path_answer(
        &mut self,
        family: path_register::PathFamily,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let argument = match &self.nodes[node.index()] {
            ProgramNode::Call { args, .. } => *args
                .first()
                .ok_or_else(|| internal_contract("a path read call with no argument"))?,
            _ => return Err(internal_contract("path read dispatch over a non-call node")),
        };
        self.raise_cont = cont;
        // The path argument may itself navigate (delpaths([path(.b)])): keep
        // the located input so a markup member step still sees name facts.
        // The owned subject is only the document getpath/delpaths rewrite.
        let body_input = input.try_clone().map_err(EngineRunError::Codec)?;
        // `getpath` is a READ family and takes the MEMOIZED materialization
        // (one materialization per loop); `delpaths` is a WRITE family by the
        // standing taxonomy and keeps the unmemoized capture (the memoized
        // cache is the read families' lane).
        let owned = match family {
            path_register::PathFamily::GetPath => self.materialize_subject_memoized(input, resources)?,
            _ => self.materialize_capture(input, resources)?,
        };
        let subject = match family {
            path_register::PathFamily::GetPath => AnswerSubject::GetPath(owned),
            _ => AnswerSubject::DelPaths(owned),
        };
        self.push_frame(GraphFrame::ArgumentAnswer {
            subject,
            out_cont: cont,
        });
        self.pending = Some(GraphTask::Eval {
            node: argument,
            input: body_input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Opens `setpath(P; V)`'s streaming argument drive: the VALUE argument
    /// is the outer loop, the PATH the inner one, every `(path, value)` pair
    /// publishing the rewritten document. This fixes the latent ordering
    /// divergence of the frozen path-outer loop: the answer is V-OUTER
    /// (`setpath((["b"],["c"]); (1,2))` is `b:1, c:1, b:2, c:2`), and an
    /// EMPTY value argument cancels the product before the path argument is
    /// ever evaluated — `setpath(error; empty)` answers nothing.
    pub(crate) fn eval_set_path(
        &mut self,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let args = self.call_arg_ids(node, "setpath dispatch over a non-call node")?;
        if args.len() != 2 {
            return Err(internal_contract("setpath call without exactly two arguments"));
        }
        self.raise_cont = cont;
        // The write families take the UNMEMOIZED materialization: the frame
        // holds the only handle, and every answer writes through a fresh clone
        // of it, so the write owns the document it rewrites (the sole-ownership
        // write law).
        let body_input = input.try_clone().map_err(EngineRunError::Codec)?;
        let path_input = body_input.try_clone().map_err(EngineRunError::Codec)?;
        let root = self.materialize_capture(input, resources)?;
        self.push_frame(GraphFrame::PathSetValue {
            root,
            path_arg: args[0],
            path_input,
            out_cont: cont,
        });
        self.pending = Some(GraphTask::Eval {
            node: args[1],
            input: body_input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Consumes one VALUE argument output of the `setpath` drive and opens the
    /// PATH argument's inner drive over it.
    pub(crate) fn consume_path_set_value(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let owned = self.materialize_capture(value, resources)?;
        let (root, path_arg, path_input, out_cont) = match self.frames.as_slice().get(index) {
            Some(GraphFrame::PathSetValue {
                root,
                path_arg,
                path_input,
                out_cont,
            }) => (
                root.clone(),
                *path_arg,
                path_input.try_clone().map_err(EngineRunError::Codec)?,
                *out_cont,
            ),
            _ => return Err(internal_contract("setpath value frame kind changed")),
        };
        self.raise_cont = out_cont;
        self.push_frame(GraphFrame::PathSetPair {
            root,
            value: owned,
            out_cont,
        });
        self.pending = Some(GraphTask::Eval {
            node: path_arg,
            input: path_input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Answers one PATH argument output of the `setpath` drive: the document
    /// rewritten at the `(path, value)` pair, routed immediately.
    pub(crate) fn consume_path_set_pair(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let path = self.materialize_capture(value, resources)?;
        let (root, update, out_cont) = match self.frames.as_slice().get(index) {
            Some(GraphFrame::PathSetPair { root, value, out_cont }) => (root.clone(), value.clone(), *out_cont),
            _ => return Err(internal_contract("setpath pair frame kind changed")),
        };
        self.raise_cont = out_cont;
        let answer = semantics_path::set_path(root, &path, update, resources)?;
        let result = self.retain_owned(answer);
        self.route(result, out_cont, resources)
    }

    /// Drives the tracked path families (`path(f)` and the native `paths`
    /// walk) over one call.
    ///
    /// A READ-only family only navigates the materialized root, so the
    /// materialization is memoized by the located input's node: a getpath loop
    /// over one document otherwise re-materializes the WHOLE document per call
    /// — the Family 1 lane (`[limit(500; paths(scalars)) as $p |
    /// getpath($p)]`).
    pub(crate) fn eval_path_walk(
        &mut self,
        family: path_register::PathFamily,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let args = self.call_arg_ids(node, "path dispatch over a non-call node")?;
        self.raise_cont = cont;
        match family {
            // The native walk is always resumable: `paths` IS the lifted
            // descend walk with the root excluded by the register's depth.
            path_register::PathFamily::Paths => {
                let slots = self.env_slot_snapshot()?;
                Ok(self.open_path_walk(input, slots, Vec::new(), false, cont))
            }
            // `path(f)` streams only when the body is a descend spine with
            // identity-preserving per-child bodies; any other body NAVIGATES
            // (`.a`) or constructs, which extends the register inside the
            // body — the nested machines cannot record that, so it stays on
            // the frozen run.
            path_register::PathFamily::Path => {
                let body = args
                    .first()
                    .copied()
                    .ok_or_else(|| internal_contract("path/1 call with no argument"))?;
                if let Some(pipeline) = self.path_walk_pipeline(body) {
                    let slots = self.env_slot_snapshot()?;
                    Ok(self.open_path_walk(input, slots, pipeline, true, cont))
                } else {
                    // The register drive: install the register and drive the
                    // body on THIS machine — a path query as an ordinary
                    // machine drive, the wave's structural claim.
                    let body_input = input.try_clone().map_err(EngineRunError::Codec)?;
                    let source_node = input.located().map(LocatedProduct::node);
                    let root = self.materialize_subject_memoized(input, resources)?;
                    let saved = self.path.take();
                    self.path = Some(path_register::PathRegister::new(root, source_node));
                    self.push_frame(GraphFrame::PathPublish {
                        saved,
                        values: Vec::new(),
                        pending: None,
                        out_cont: cont,
                        finish: PathFinish::Stream,
                        assign: None,
                    });
                    self.pending = Some(GraphTask::Eval {
                        node: body,
                        input: body_input,
                        cont: self.frames.len(),
                    });
                    Ok(None)
                }
            }
            path_register::PathFamily::GetPath
            | path_register::PathFamily::SetPath
            | path_register::PathFamily::DelPaths => {
                Err(internal_contract("path walk drive reached a read/write family"))
            }
        }
    }

    /// The binder-slot snapshot the walker's per-child body machines are
    /// seeded with: every env entry cloned as a handle, so a located slot
    /// stays located and a body machine pays no materialization for it.
    ///
    /// An empty env is returned as an empty vec — no per-child copy.
    pub(crate) fn env_slot_snapshot(&self) -> Result<Vec<Option<EngineResult<'source>>>, EngineRunError> {
        if self.env.as_slice().is_empty() {
            return Ok(Vec::new());
        }
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(self.env.as_slice().len())
            .map_err(|_| EngineRunError::allocation_failure())?;
        for held in self.env.as_slice() {
            slots.push(Some(held.try_clone().map_err(EngineRunError::Codec)?));
        }
        Ok(slots)
    }

    /// The streaming body pipeline of a `path(f)` call when the body is a
    /// descend spine with identity-preserving per-child bodies: matches
    /// `paths/1`'s lowering `path(.. | select(f))` and `path(..)` itself. The
    /// spine is the `FlatMap` chain whose bottom upstream is the descend
    /// stage; each body must emit only its input
    /// ([`Self::is_identity_preserving`]), because the walker publishes the
    /// WALK's register, never a body-constructed value.
    pub(crate) fn path_walk_pipeline(&self, body: ProgramNodeId) -> Option<Vec<ProgramNodeId>> {
        let mut pipeline = Vec::new();
        if self.collect_walk_pipeline(body, &mut pipeline) {
            Some(pipeline)
        } else {
            None
        }
    }

    /// One level of the pipeline spine walk: the `FlatMap` chain is descended
    /// to its bottom, collecting each body on the way back up; the bottom
    /// must be a bare `..` stage.
    pub(crate) fn collect_walk_pipeline(&self, node: ProgramNodeId, pipeline: &mut Vec<ProgramNodeId>) -> bool {
        match &self.nodes[node.index()] {
            ProgramNode::FlatMap { upstream, body } => {
                if !self.collect_walk_pipeline(*upstream, pipeline) {
                    return false;
                }
                if !Self::is_identity_preserving(*body, self.nodes) {
                    return false;
                }
                pipeline.push(*body);
                true
            }
            // The descend stage, exactly as the lowering builds it: a single
            // `..` step and nothing before or after it (a prefixed or
            // post-stepped stage would need steps applied around the descend).
            ProgramNode::Stage {
                start: StageStart::Current,
                steps,
            } => steps.len() == 1 && matches!(steps[0].access(), StepAccess::Descend),
            _ => false,
        }
    }

    /// Whether a node emits ONLY its input (possibly filtered, possibly
    /// repeatedly), never a navigation or construction result: every output
    /// is the walked child itself, so the register stays the walk's own.
    /// `select(f)` and the kind filters emit the admitted input; a bare `.`
    /// stage emits it once; a pipe or choice of identity-preserving nodes
    /// stays identity-preserving.
    pub(crate) fn is_identity_preserving(node: ProgramNodeId, nodes: &[ProgramNode]) -> bool {
        match &nodes[node.index()] {
            ProgramNode::Call { payload, .. } => {
                matches!(payload, Evaluator::Select | Evaluator::Kind(_))
            }
            ProgramNode::Stage {
                start: StageStart::Current,
                steps,
            } => steps.is_empty(),
            ProgramNode::FlatMap { upstream, body } => {
                Self::is_identity_preserving(*upstream, nodes) && Self::is_identity_preserving(*body, nodes)
            }
            ProgramNode::Choice { left, right } => {
                Self::is_identity_preserving(*left, nodes) && Self::is_identity_preserving(*right, nodes)
            }
            _ => false,
        }
    }

    /// Opens the resumable walk producer frame over the call's own input —
    /// LOCATED, never a whole-document materialization — so a countdown cut
    /// stops paying for the document with the walk.
    pub(crate) fn open_path_walk(
        &mut self,
        root: EngineResult<'source>,
        slots: Vec<Option<EngineResult<'source>>>,
        pipeline: Vec<ProgramNodeId>,
        include_root: bool,
        cont: usize,
    ) -> Option<EngineResult<'source>> {
        let stages = pipeline
            .iter()
            .map(|node| Self::resolve_walk_stage(*node, self.nodes))
            .collect();
        let walk = path_register::PathWalk::new(root);
        self.push_frame(GraphFrame::PathWalkEmit {
            walk,
            pipeline,
            stages,
            machines: Vec::new(),
            reuse: None,
            slots,
            include_root,
            out_cont: cont,
        });
        None
    }

    /// Classify one pipeline stage once per walk, not per walked child.
    pub(crate) fn resolve_walk_stage(node: ProgramNodeId, nodes: &[ProgramNode]) -> WalkStage {
        match &nodes[node.index()] {
            ProgramNode::Call { payload, args, .. } => match payload {
                Evaluator::Kind(filter) => WalkStage::Kind {
                    filter: *filter,
                    through_select: false,
                },
                Evaluator::Select => match args.first().and_then(|pred| {
                    let ProgramNode::Call { payload, .. } = &nodes[pred.index()] else {
                        return None;
                    };
                    match payload {
                        Evaluator::Kind(filter) => Some(*filter),
                        _ => None,
                    }
                }) {
                    Some(filter) => WalkStage::Kind {
                        filter,
                        through_select: true,
                    },
                    None => WalkStage::Machine(node),
                },
                _ => WalkStage::Machine(node),
            },
            _ => WalkStage::Machine(node),
        }
    }
}
