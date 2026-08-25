//! Binary, call, constructor, and remaining node evaluators.
//!
//! One job: the graph-machine methods that are not seed/dispatch, not
//! bind/fold, not path-family, and not emission routing. Path-family
//! handoff lives in [`super::pathmode`]; `route` lives in [`super::route`].

#[allow(clippy::wildcard_imports)]
use super::*;

/// Joins the leftmost concat piece with the already-materialized right pieces
/// into one string. Holes already ran `tostring`; a non-string is a contract
/// break, never a user-facing `+` mismatch.
fn join_concat_pieces(head: &Value, suffix: &[Value]) -> Result<Value, EngineRunError> {
    let head_text = concat_string(head)?;
    if suffix.is_empty() {
        return Ok(head.clone());
    }
    let mut total = head_text.len();
    let mut rest = Vec::new();
    rest.try_reserve(suffix.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for piece in suffix {
        let text = concat_string(piece)?;
        total = total
            .checked_add(text.len())
            .ok_or_else(EngineRunError::allocation_failure)?;
        rest.push(text);
    }
    let mut buf = String::new();
    buf.try_reserve_exact(total)
        .map_err(|_| EngineRunError::allocation_failure())?;
    buf.push_str(head_text);
    for text in rest {
        buf.push_str(text);
    }
    Value::try_string(&buf).map_err(|_| EngineRunError::allocation_failure())
}

fn concat_string(value: &Value) -> Result<&str, EngineRunError> {
    match value.untagged() {
        Value::String(text) => Ok(text.as_str()),
        _ => Err(EngineRunError::internal_contract(
            "a concat part was not a string after tostring",
        )),
    }
}

impl<'program, 'source> GraphMachine<'program, 'source> {
    /// Evaluates a `Binary` operator over `input`: retain the original input in a
    /// `BinaryRight` consumer frame, then drive the RIGHT operand graph (the outer
    /// Cartesian loop) over the original input. Each right output starts a left
    /// iteration paired under `op`.
    #[allow(
        clippy::too_many_arguments,
        reason = "shape is a compile-time fact passed beside the operand ids"
    )]
    pub(crate) fn eval_binary(
        &mut self,
        op: BinaryKind,
        left: ProgramNodeId,
        right: ProgramNodeId,
        shape: BinaryShape,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        // `. OP <single-valued operand>` needs NO frame: both operands produce
        // exactly one value, the left one being the input itself. Owned and
        // located inputs take the same exit; located materializes once first.
        // Shape is a compile-time fact (mark_binary_shapes after fuse).
        match shape {
            BinaryShape::IdentityLiteral => {
                let literal = constant_operand(self.nodes, right)
                    .ok_or_else(|| internal_contract("IdentityLiteral binary right is not a literal"))?;
                let left_owned = match input {
                    EngineResult::Owned(value) => value,
                    located @ EngineResult::Located(_) => {
                        let literal = literal.clone();
                        let left_owned = self.materialize_capture(located, resources)?;
                        return self.apply_binary_pair(op, left_owned, &literal, cont, resources);
                    }
                };
                return self.apply_binary_pair(op, left_owned, literal, cont, resources);
            }
            BinaryShape::IdentitySlot(slot) => {
                if let Some(operand) = self
                    .env
                    .as_slice()
                    .get(slot as usize)
                    .and_then(EngineResult::owned_value)
                    .cloned()
                {
                    let left_owned = match input {
                        EngineResult::Owned(value) => value,
                        located @ EngineResult::Located(_) => self.materialize_capture(located, resources)?,
                    };
                    return self.apply_binary_pair(op, left_owned, &operand, cont, resources);
                }
                // Located or missing slot: the framed path charges residency.
            }
            BinaryShape::Framed => {}
        }
        // `f OP <single-valued operand>`: the right operand drives the outer
        // Cartesian loop the `BinaryRight` frame exists for, and that loop has
        // one iteration whose value is readable here. Pairing it now retires
        // that frame, the input re-borrow and one routing walk — `.stock * 2`
        // and `select(.stock > 50)` are this shape.
        if let Some(operand) = constant_operand(self.nodes, right).or_else(|| self.slot_operand(right)) {
            let right = operand.clone();
            // The binary's operands are subexpressions: frozen.
            self.path_enter_frozen();
            self.push_frame(GraphFrame::BinaryLeft {
                op,
                right,
                right_handle: None,
                out_cont: cont,
            });
            self.pending = Some(GraphTask::Eval {
                node: left,
                input,
                cont: self.frames.len(),
            });
            return Ok(None);
        }
        // The left operand re-runs over the SAME input per right output: re-borrow
        // it now (fallible at the fork), retaining the original for the right eval.
        // The binary's operands are frozen subexpressions.
        self.path_enter_frozen();
        let saved = input.try_clone().map_err(EngineRunError::Codec)?;
        self.push_frame(GraphFrame::BinaryRight {
            op,
            left,
            input: saved,
            out_cont: cont,
        });
        self.pending = Some(GraphTask::Eval {
            node: right,
            input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// The OWNED value a step-less `Variable` stage produces, read off the
    /// environment instead of evaluated: one output, no input read, no fan-out,
    /// no failure but the clone. [`descend_borrowed`] over an empty step list is
    /// exactly `Leaf(clone(root))`, which is that value and nothing else.
    ///
    /// A LOCATED slot declines and is left to the framed path, which materializes
    /// it where the operand's residency can be charged to the frame that holds
    /// it. An out-of-range slot declines too and raises its contract error there.
    pub(crate) fn slot_operand(&self, node: ProgramNodeId) -> Option<&Value> {
        match &self.nodes[node.index()] {
            ProgramNode::Stage {
                start: StageStart::Variable(slot),
                steps,
            } if steps.is_empty() => self.env.as_slice().get(*slot as usize)?.owned_value(),
            _ => None,
        }
    }

    /// Applies one complete `(left, right)` binary pair at the op barrier with no
    /// frame standing under it — the frame-free twin of
    /// [`Self::consume_binary_left`]'s tail, for pairs [`Self::eval_binary`]
    /// resolved without enumerating anything. The failure arm is spelled out
    /// twice rather than shared because the framed one reads its right operand
    /// back out of `self.frames` while `self` is borrowed, and routing it through
    /// a common helper would cost a clone of the operand on every SUCCESSFUL
    /// pair — the case this path exists to make cheap.
    pub(crate) fn apply_binary_pair(
        &mut self,
        op: BinaryKind,
        left: Value,
        right: &Value,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        mismatch::note_cross_kind_ordering(op, &left, right, resources)?;
        match binary::apply_owned(op, left, right, resources) {
            Ok(value) => self.route(EngineResult::owned(value), cont, resources),
            Err((error, left)) => {
                // Set BEFORE the mapping: a depth cap leaves through `?` as a
                // raised value, and a raised value needs its catch position.
                self.raise_cont = cont;
                let failure = ArithFailure::from_binary(error, resources)?;
                Err(EngineRunError::Arithmetic {
                    failure,
                    left: message::dump_trunc_owned(&left)?,
                    right: message::dump_trunc_owned(right)?,
                })
            }
        }
    }

    /// Consumes one right output for a `Binary`: materialize it (once, reused
    /// across the left iteration), then drive the left operand graph over a
    /// re-borrow of the retained original input, routing left outputs to a fresh
    /// `BinaryLeft` frame.
    ///
    /// The operand's residency travels INTO that frame, so it releases when the
    /// left iteration ends and the frame pops — the scope the bytes actually have.
    ///
    /// The retained input is re-BORROWED while the right operand can still emit
    /// and MOVED once it cannot ([`Self::producer_spent_above`]), the same law
    /// [`Self::consume_bind_body`] follows and for the same reason: inside a fold
    /// of `+` the retained input IS the accumulator, and a second live handle
    /// turns [`binary::apply_owned`]'s in-place extension back into the
    /// whole-container rebuild it exists to avoid.
    pub(crate) fn consume_binary_right(
        &mut self,
        index: usize,
        op: BinaryKind,
        left: ProgramNodeId,
        out_cont: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let spent = self.producer_spent_above(index);
        let right_handle = value.located().map(LocatedProduct::node);
        let right = self.materialize_operand(value, resources)?;
        let input = {
            let GraphFrame::BinaryRight { input, .. } = self.routed_frame_mut(index) else {
                unreachable!("emission walk already classified BinaryRight");
            };
            take_or_borrow(input, spent)?
        };
        // The right operand's frozen scope ends; the left's opens.
        self.path_exit_frozen();
        self.path_enter_frozen();
        self.push_frame(GraphFrame::BinaryLeft {
            op,
            right,
            right_handle,
            out_cont,
        });
        self.pending = Some(GraphTask::Eval {
            node: left,
            input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Consumes one left output for a `Binary`: materialize it, apply `op` to the
    /// `(left, right)` pair at the op barrier, and route the owned result through
    /// the operator's out-continuation. A typed operator failure aborts the run
    /// after any already-emitted pairs.
    ///
    /// The pair is applied through [`binary::apply_owned`], which CONSUMES the
    /// left operand so `+` can extend it rather than build a third container.
    ///
    /// The operand's residency is a local, released when this pair is done, and
    /// that stays right when the result IS the extended left operand: the local
    /// charge covers the handle word this transition holds, while the operand's
    /// own payload bytes are charged on its allocation and travel with it. Every
    /// result already retained operand allocations — `Value::try_clone` is a
    /// refcount bump — so no accounting law changes here.
    pub(crate) fn consume_binary_left(
        &mut self,
        index: usize,
        op: BinaryKind,
        out_cont: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        // The allocation-identity short-circuit: the test answers true for two
        // handles that share one allocation before recursing, so when
        // the LEFT output is still located at the very node the RIGHT operand
        // was materialized from, the pair is identical without materializing it
        // (no left charge is created; the right's stays frame-owned). The
        // top-level NaN carve-out is exactly `semantic_eq`'s: a NaN double
        // carries no allocation to be identical to, so it always compares.
        let identity = {
            let GraphFrame::BinaryLeft {
                right, right_handle, ..
            } = self.routed_frame(index)
            else {
                unreachable!("emission walk already classified BinaryLeft");
            };
            let same_node = matches!(
                value.located(),
                Some(located) if *right_handle == Some(located.node())
            );
            let top_level_nan = matches!(
                right.untagged(),
                Value::Number(number)
                    if number.as_float().is_some_and(|f| f.get().is_nan())
            );
            if (op == BinaryKind::Equal || op == BinaryKind::NotEqual) && same_node && !top_level_nan {
                Some(op == BinaryKind::Equal)
            } else {
                None
            }
        };
        if let Some(result) = identity {
            // The operands' frozen scope ends before the result routes.
            self.path_exit_frozen();
            return self.route(EngineResult::owned(Value::Bool(result)), out_cont, resources);
        }
        let left = self.materialize_operand(value, resources)?;
        let result = {
            let GraphFrame::BinaryLeft { right, .. } = self.routed_frame(index) else {
                unreachable!("emission walk already classified BinaryLeft");
            };
            // The cross-kind ordering cell fires at the PROGRAM
            // comparison node; builtin internals never route an ordering
            // op through this frame (they call the comparator laws
            // directly), so `sort` over a mixed array stays immune.
            mismatch::note_cross_kind_ordering(op, &left, right, resources)?;
            binary::apply_owned(op, left, right, resources)
        };
        let value = match result {
            Ok(value) => value,
            Err((error, left)) => {
                // Set BEFORE the mapping: a depth cap leaves through `?` as a
                // raised value, and a raised value needs its catch position.
                self.raise_cont = out_cont;
                let failure = ArithFailure::from_binary(error, resources)?;
                // Cold path only: render the operands' bounded compact JSON for the
                // catch-visible / CLI message (never on a successful pair).
                let left_json = message::dump_trunc_owned(&left)?;
                let GraphFrame::BinaryLeft { right, .. } = self.routed_frame(index) else {
                    unreachable!("emission walk already classified BinaryLeft");
                };
                let right_json = message::dump_trunc_owned(right)?;
                return Err(EngineRunError::Arithmetic {
                    failure,
                    left: left_json,
                    right: right_json,
                });
            }
        };
        // The operands' frozen scope ends before the result routes.
        self.path_exit_frozen();
        self.route(EngineResult::owned(value), out_cont, resources)
    }

    /// Evaluates a `Concat` over `input`: drive the RIGHTMOST part (the outer
    /// Cartesian loop) first. Each of its string outputs either starts an
    /// inner Concat of the remaining left parts or, when this is the only
    /// part, is the result. Leftmost varies fastest — the same order a
    /// left-associative `+` chain produced.
    pub(crate) fn eval_concat(
        &mut self,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let parts = match &self.nodes[node.index()] {
            ProgramNode::Concat { parts } => parts.as_slice(),
            _ => {
                return Err(internal_contract("graph machine evaluated a non-Concat as Concat"));
            }
        };
        if parts.is_empty() {
            return Err(internal_contract("a Concat node with no parts"));
        }
        let at = parts.len() - 1;
        let first = parts[at];
        self.path_enter_frozen();
        let saved = input.try_clone().map_err(EngineRunError::Codec)?;
        self.push_frame(GraphFrame::ConcatPart {
            node,
            at,
            suffix: Vec::new(),
            input: saved,
            out_cont: cont,
        });
        self.pending = Some(GraphTask::Eval {
            node: first,
            input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Consumes one part output for a `Concat`: materialize it as a string
    /// (holes already ran `tostring`). The leftmost part joins every collected
    /// piece into one buffer; every other part nests a Concat over the
    /// remaining left parts with this piece prepended to the suffix.
    pub(crate) fn consume_concat(
        &mut self,
        index: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let piece = self.materialize_operand(value, resources)?;
        let (at, node, out_cont, spent) = {
            let GraphFrame::ConcatPart { at, node, out_cont, .. } = self.routed_frame(index) else {
                unreachable!("emission walk already classified ConcatPart");
            };
            (*at, *node, *out_cont, self.producer_spent_above(index))
        };
        if at == 0 {
            let joined = {
                let GraphFrame::ConcatPart { suffix, .. } = self.routed_frame(index) else {
                    unreachable!("emission walk already classified ConcatPart");
                };
                join_concat_pieces(&piece, suffix)?
            };
            self.path_exit_frozen();
            return self.route(EngineResult::owned(joined), out_cont, resources);
        }
        let inner = match &self.nodes[node.index()] {
            ProgramNode::Concat { parts } => parts
                .get(at - 1)
                .copied()
                .ok_or_else(|| internal_contract("a Concat part index walked off the parts"))?,
            _ => return Err(internal_contract("ConcatPart lost its Concat node")),
        };
        let (suffix, input) = {
            let GraphFrame::ConcatPart { suffix, input, .. } = self.routed_frame_mut(index) else {
                unreachable!("emission walk already classified ConcatPart");
            };
            let mut next = Vec::new();
            next.try_reserve(suffix.len().saturating_add(1))
                .map_err(|_| EngineRunError::allocation_failure())?;
            next.push(piece);
            next.extend(suffix.iter().cloned());
            (next, take_or_borrow(input, spent)?)
        };
        self.path_exit_frozen();
        self.path_enter_frozen();
        self.push_frame(GraphFrame::ConcatPart {
            node,
            at: at - 1,
            suffix,
            input: input.try_clone().map_err(EngineRunError::Codec)?,
            out_cont,
        });
        self.pending = Some(GraphTask::Eval {
            node: inner,
            input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Evaluates a `Conditional` over `input`: retain the original input in a
    /// `ConditionalBody` consumer frame, then drive the condition graph over a
    /// re-borrow of that input. Each condition output selects an arm over a fresh
    /// re-borrow of the retained original input.
    pub(crate) fn eval_conditional(
        &mut self,
        condition: ProgramNodeId,
        consequent: ProgramNodeId,
        alternative: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        self.path_enter_frozen(); // the condition is a frozen subexpression
        // The selected arm runs over the SAME input the condition sees: re-borrow
        // it now (fallible at the fork), retaining the original for the arms.
        let condition_input = input.try_clone().map_err(EngineRunError::Codec)?;
        self.push_frame(GraphFrame::ConditionalBody {
            consequent,
            alternative,
            input,
            out_cont: cont,
        });
        self.pending = Some(GraphTask::Eval {
            node: condition,
            input: condition_input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Consumes one condition output for a `Conditional`: a truthy output selects
    /// the consequent, a falsy one the alternative, each evaluated over a fresh
    /// re-borrow of the frame's retained original input and routed through the
    /// conditional's out-continuation.
    pub(crate) fn consume_conditional(
        &mut self,
        index: usize,
        consequent: ProgramNodeId,
        alternative: ProgramNodeId,
        out_cont: usize,
        value: &EngineResult<'source>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let truthy = truth::is_truthy(value).map_err(|_| internal_contract("conditional truth check failed"))?;
        let arm = if truthy { consequent } else { alternative };
        let input = {
            let GraphFrame::ConditionalBody { input, .. } = self.routed_frame(index) else {
                unreachable!("emission walk already classified ConditionalBody");
            };
            input.try_clone().map_err(EngineRunError::Codec)?
        };
        // The condition's frozen scope ends when its output is consumed.
        self.path_exit_frozen();
        self.pending = Some(GraphTask::Eval {
            node: arm,
            input,
            cont: out_cont,
        });
        Ok(None)
    }

    /// Evaluates an `Alternative` (`//`) over `input`: retain the original input in
    /// an `AlternativeLeft` filtering-pass-through frame, then drive the left graph
    /// over a re-borrow of that input. Truthy left outputs pass through; the right
    /// fallback is seeded only if the left completes with zero truthy outputs.
    pub(crate) fn eval_alternative(
        &mut self,
        left: ProgramNodeId,
        right: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        // The right fallback runs over the SAME input the left filtered: re-borrow
        // it now (fallible at the fork), retaining the original for the left drive.
        let left_input = input.try_clone().map_err(EngineRunError::Codec)?;
        self.push_frame(GraphFrame::AlternativeLeft {
            right,
            input,
            had_truthy: false,
            out_cont: cont,
        });
        // The alternative's LEFT is the suppression region: the
        // miss the fallback handles fires no event. The frame's whole
        // lifecycle — the left drive and the fallback seed — brackets the
        // region, so the depth is entered here and exited wherever the frame
        // pops (advance's arms, the raise walk, the spent trim, the cuts).
        resources.enter_mismatch_suppression();
        self.pending = Some(GraphTask::Eval {
            node: left,
            input: left_input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Consumes one left output for an `Alternative`: a truthy output is re-emitted
    /// upward through the alternative's out-continuation (recording that a truthy
    /// output was seen); a falsy one is swallowed with no output.
    pub(crate) fn consume_alternative_left(
        &mut self,
        index: usize,
        out_cont: usize,
        value: EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        if !truth::is_truthy(&value).map_err(|_| internal_contract("alternative truth check failed"))? {
            // A falsy left output is swallowed; the had-truthy flag is unchanged.
            return Ok(None);
        }
        let GraphFrame::AlternativeLeft { had_truthy, .. } = self.routed_frame_mut(index) else {
            unreachable!("emission walk already classified AlternativeLeft");
        };
        *had_truthy = true;
        // A truthy output has LEFT the alternative: its consumers (a bind
        // body, a pipe body) are not inside the suppression region the strict
        // dial draws around the alternative's operands, so the depth they
        // would otherwise sit under is released before the route. The left's
        // OWN remaining evaluation — a multi-output left that emits another
        // value after this one — resumes released (a recorded narrow residual:
        // `(null + 1, .b) // 5` fires `.b`'s cell where the structural law
        // would suppress it). The alternative frame's own pop stays balanced
        // by this release: `exit_mismatch_suppression` saturates at zero.
        resources.exit_mismatch_suppression();
        self.route(value, out_cont, resources)
    }

    /// Evaluates a `Logical` (`and`/`or`) over `input`: retain the original input in
    /// a `LogicalLeft` consumer frame (the outer loop), then drive the LEFT operand
    /// graph over a re-borrow of that input. Each left output short-circuits or
    /// drives the right operand.
    pub(crate) fn eval_logical(
        &mut self,
        operator: LogicalOp,
        left: ProgramNodeId,
        right: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        // The right operand re-runs over the SAME input per non-short-circuiting
        // left output: re-borrow it now (fallible at the fork), retaining the
        // original for the right evaluations.
        let left_input = input.try_clone().map_err(EngineRunError::Codec)?;
        self.push_frame(GraphFrame::LogicalLeft {
            operator,
            right,
            input,
            out_cont: cont,
        });
        self.pending = Some(GraphTask::Eval {
            node: left,
            input: left_input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Consumes one left output for a `Logical`: on a short-circuit (`and` + falsy,
    /// or `or` + truthy) emit the boolean of the LEFT's truthiness; otherwise drive
    /// the right operand over a re-borrow of the retained original input, routing
    /// its outputs to a fresh `LogicalRight` booleanizing frame.
    pub(crate) fn consume_logical_left(
        &mut self,
        index: usize,
        operator: LogicalOp,
        right: ProgramNodeId,
        out_cont: usize,
        value: &EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let truthy = truth::is_truthy(value).map_err(|_| internal_contract("logical truth check failed"))?;
        let input = {
            let GraphFrame::LogicalLeft { input, .. } = self.routed_frame(index) else {
                unreachable!("emission walk already classified LogicalLeft");
            };
            input.try_clone().map_err(EngineRunError::Codec)?
        };
        let short_circuits = match operator {
            LogicalOp::And => !truthy,
            LogicalOp::Or => truthy,
        };
        if short_circuits {
            // The short-circuit output is the boolean of the LEFT's truthiness.
            return self.route(EngineResult::owned(Value::Bool(truthy)), out_cont, resources);
        }
        self.push_frame(GraphFrame::LogicalRight { out_cont });
        self.pending = Some(GraphTask::Eval {
            node: right,
            input,
            cont: self.frames.len(),
        });
        Ok(None)
    }

    /// Consumes one right output for a `Logical`: booleanize it (the RIGHT's
    /// truthiness) and route the owned boolean through the operator's
    /// out-continuation.
    pub(crate) fn consume_logical_right(
        &mut self,
        out_cont: usize,
        value: &EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let truthy = truth::is_truthy(value).map_err(|_| internal_contract("logical truth check failed"))?;
        self.route(EngineResult::owned(Value::Bool(truthy)), out_cont, resources)
    }

    /// Dispatches one resolved `Call` node over `input`. `length`/`keys` are pure
    /// evaluators producing exactly one routed output (their owned results are
    /// ledger-charged); `select` sets up its predicate consumer frame. Every
    /// stored `Call` is an `Evaluator` — `Lowering` builtins never reach the
    /// arena.
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per evaluator family: the call dispatch IS the table, and \
                  splitting it would hide which evaluators are covered"
    )]
    pub(crate) fn eval_call(
        &mut self,
        payload: Evaluator,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        // A builtin-domain raise (no-length / no-keys / `error/0`) fails a value
        // routing at this call's `cont`.
        self.raise_cont = cont;
        match payload {
            Evaluator::Length => {
                let result = core_builtins::length(&input, resources)?;
                let result = self.retain_owned(result);
                self.route(result, cont, resources)
            }
            Evaluator::Keys => {
                let value = core_builtins::keys(&input, core_builtins::KeyOrder::Sorted, resources)?;
                let result = self.retain_owned(value);
                self.route(result, cont, resources)
            }
            Evaluator::Type => {
                let value = core_builtins::type_name(&input, resources)?;
                let result = self.retain_owned(value);
                self.route(result, cont, resources)
            }
            // `tag` reads one intrinsic fact of the input node, so a LOCATED
            // input is never materialized to answer it.
            Evaluator::Tag => {
                let value = core_builtins::tag_name(&input, resources)?;
                let result = self.retain_owned(value);
                self.route(result, cont, resources)
            }
            Evaluator::JsonFacts => {
                let value = facts_builtins::json_facts_cached(&input, resources, &mut self.json_facts_cache)?;
                let result = self.retain_owned(value);
                self.route(result, cont, resources)
            }
            // `_negate` reads the input's number and nothing else, so a LOCATED
            // operand is re-signed from its borrowed representation rather than
            // materialized: the owned round trip canonicalizes a retained decimal
            // and `-1.500` would answer `-1.5`.
            Evaluator::Negate => {
                let value = core_builtins::negate(&input, resources)?;
                let result = self.retain_owned(value);
                self.route(result, cont, resources)
            }
            Evaluator::Select => self.eval_select(node, input, cont, resources),
            // The kind filters (`objects`, `arrays`, …): the admitted input is
            // routed UNCHANGED — no predicate frame, no re-borrow, and no owned
            // copy, so a located value stays a located handle — and a rejected
            // one simply produces nothing.
            Evaluator::Kind(filter) => {
                if kind_builtins::admits(filter, &input, resources)? {
                    self.route(input, cont, resources)
                } else {
                    Ok(None)
                }
            }
            Evaluator::Not => {
                // `not` is the boolean of the input's FALSINESS: `false`/`null`
                // input → `true`, every other value (including `0`, `""`) → `false`.
                let truthy = truth::is_truthy(&input).map_err(|_| internal_contract("not truth check failed"))?;
                let result = self.retain_owned(Value::Bool(!truthy));
                self.route(result, cont, resources)
            }
            Evaluator::ErrorZero => {
                // `error/0` raises the current input as the error value; it never
                // routes a normal output. The error walks to the nearest `try`.
                let value = self.materialize_capture(input, resources)?;
                Err(EngineRunError::Raised(value))
            }
            Evaluator::ErrorOne => self.eval_error_arg(node, input, cont),
            // The path-family evaluators: the tracked families share the
            // walker drive, the reads answer per argument output.
            Evaluator::Path => self.eval_path_family(path_register::PathFamily::Path, node, input, cont, resources),
            Evaluator::Paths => self.eval_path_family(path_register::PathFamily::Paths, node, input, cont, resources),
            Evaluator::GetPath => {
                self.eval_path_family(path_register::PathFamily::GetPath, node, input, cont, resources)
            }
            Evaluator::SetPath => {
                self.eval_path_family(path_register::PathFamily::SetPath, node, input, cont, resources)
            }
            Evaluator::DelPaths => {
                self.eval_path_family(path_register::PathFamily::DelPaths, node, input, cont, resources)
            }
            // The codec-native selector seam: `xpath/1` runs the xml.xpath@1
            // profile and `css/1` runs the html.css@1 profile over the located
            // document authority of the input. Both are pure, deterministic
            // laws; a missing authority, a format mismatch, a compile
            // rejection, or a budget exhaustion is a catchable string raise.
            Evaluator::XPath => self.eval_selector(selector_builtins::SelectorLaw::XPath, node, input, cont, resources),
            Evaluator::Css => self.eval_selector(selector_builtins::SelectorLaw::Css, node, input, cont, resources),
            // The arity-0 ordering forms and the value-shaping evaluators are
            // pure owned-value laws: materialize once, compute, route. Their
            // rejections are raised before anything is published.
            Evaluator::Whole(form) => self.eval_owned_law(input, cont, resources, |subject, resources| {
                order_builtins::whole(form, subject, resources)
            }),
            Evaluator::Reverse => self.eval_owned_law(input, cont, resources, order_builtins::reverse),
            Evaluator::ToString => {
                // A STRING passes through unchanged, and a located one stays
                // located: `tostring` is the identity on strings, so there is
                // nothing to render and nothing to copy.
                //
                // A TAGGED string is excluded from that shortcut, because it is
                // not identity there: `text_builtins::tostring` — the law this
                // arm is an optimization OF — begins with `input.untagged()`,
                // so the builtin's own answer is a CORE string. `type` is
                // payload-transparent, so the kind filter alone cannot see the
                // difference, and taking the shortcut published a value still
                // carrying a tag, which the JSON encoder then refused outright.
                if kind_builtins::admits(kind_builtins::KindFilter::String, &input, resources)?
                    && !carries_non_core_tag(&input)?
                {
                    return self.route(input, cont, resources);
                }
                self.eval_owned_law(input, cont, resources, text_builtins::tostring)
            }
            Evaluator::ToJson => self.eval_owned_law(input, cont, resources, text_builtins::tojson),
            Evaluator::ToEntries => self.eval_owned_law(input, cont, resources, entries_builtins::to_entries),
            Evaluator::FromEntries => self.eval_owned_law(input, cont, resources, entries_builtins::from_entries),
            // `keys_unsorted` reads the input WITHOUT materializing it, which is
            // the whole point of the conservative `Subtree` transfer it declares.
            Evaluator::KeysUnsorted => {
                let value = core_builtins::keys(&input, core_builtins::KeyOrder::Insertion, resources)?;
                let result = self.retain_owned(value);
                self.route(result, cont, resources)
            }
            // The two argument-driven families open a frame instead: their
            // argument is an ordinary filter over the same input, and every
            // output it produces contributes.
            Evaluator::Keyed(mode) => self.eval_keyed(mode, node, input, cont, resources),
            Evaluator::BSearch => self.eval_answer(AnswerForm::BinarySearch, node, input, cont, resources),
            Evaluator::Add => self.eval_owned_law(input, cont, resources, reshape_builtins::add),
            // `flatten` splits on ARITY, not on identity: the depth-bounded form
            // answers once per depth its argument yields, while the unbounded
            // form has no argument to drive and is an ordinary owned law.
            Evaluator::Flatten => match &self.nodes[node.index()] {
                ProgramNode::Call { args, .. } if args.is_empty() => {
                    self.eval_owned_law(input, cont, resources, |subject, resources| {
                        reshape_builtins::flatten(subject, None, resources)
                    })
                }
                ProgramNode::Call { .. } => self.eval_answer(AnswerForm::Flatten, node, input, cont, resources),
                _ => Err(internal_contract("flatten dispatch over a non-call node")),
            },
            // `join` answers once per SEPARATOR its argument yields, because
            // the separator is bound outside the fold — `["a","b"] |
            // [join("-","+")]` is two joins of the same input, not one join of
            // two separators.
            Evaluator::Join => self.eval_answer(AnswerForm::Join, node, input, cont, resources),
            Evaluator::Format => self.eval_answer(AnswerForm::Format, node, input, cont, resources),
            Evaluator::Text(law) => self.eval_answer(AnswerForm::Text(law), node, input, cont, resources),
            Evaluator::Scalar(law) => self.eval_owned_law(input, cont, resources, move |subject, resources| {
                string_builtins::apply(law, subject, resources)
            }),
            // The math family splits on SHAPE, not on name. The /0 forms are
            // pure owned-value laws — one output per input, with `nan`/
            // `infinite` ignoring the input entirely and the `isnan` quartet
            // answering `false` for a non-number without raising. The /2 and
            // /3 forms drive their filter arguments over the SAME input in
            // the right-outer order and ignore the piped value once every
            // parenthesized argument is present.
            Evaluator::Math(evaluator) => self.eval_math(evaluator, node, input, cont, resources),
            // The date/time family splits on SHAPE: the /0 forms are owned
            // value laws (`now` ignores the piped value entirely and publishes
            // the wall clock), and the /1 format laws answer once per output
            // of their format argument over the same input, driven by the
            // argument-answer frame.
            Evaluator::Time(evaluator) => self.eval_time(evaluator, node, input, cont, resources),
            // The regex family evaluates its pattern/flags argument filters
            // eagerly over the same input (the nested-machine drive), and
            // `sub`/`gsub` evaluate their REPLACEMENT filter once per match
            // with dot = the capture object. The outputs stream out of a
            // producer frame.
            Evaluator::Regex(evaluator) => self.eval_regex(evaluator, node, input, cont, resources),
            // The misc riders: `builtins`/`have_decnum` publish owned values,
            // and `debug` passes the piped value through unchanged (`/1` after
            // its message argument has run).
            Evaluator::Rider(evaluator) => self.eval_rider(evaluator, node, input, cont, resources),
            // The host-state/process family: every current law ignores the piped
            // value and publishes one owned value read from the request's
            // environment snapshot, or terminates the run (`halt`/`halt_error`).
            Evaluator::Process(evaluator) => self.eval_process(evaluator, node, input, cont, resources),
            // The streaming utilities: `tostream` walks the owned input and
            // publishes the ordered pair/marker stream; `fromstream`/
            // `truncate_stream` run their argument filter and transform its
            // outputs. All three stream out of a `PathEmit` producer frame.
            Evaluator::Streams(evaluator) => self.eval_streams(evaluator, node, input, cont, resources),
            // The jqf extension families: pure value laws over the input (or
            // over the first output of their filter arguments, the
            // argument-evaluation law).
            #[cfg(feature = "ext-hash")]
            Evaluator::Extension(law) => self.eval_extension(law, node, input, cont, resources),
            // The DIFF verb: evaluate its two filter arguments (the old and
            // the new document) over the input, then apply the path-keyed
            // semantic diff law.
            Evaluator::Diff => self.eval_diff(node, input, cont, resources),
            // The ANALYTICS family: sample/shuffle are impure value laws over
            // the piped array (shuffle unary, sample with its count argument
            // evaluated by the ordinary argument law); fill_forward is a pure
            // copy law.
            #[cfg(feature = "ext-hash")]
            Evaluator::Analytics(law) => self.eval_analytics(law, node, input, cont, resources),
            // The RAND family: uniform floats/integers and uniform element
            // choice. The unseeded forms are impure effects; `rand(seed)` is
            // the deterministic seeded exception.
            #[cfg(feature = "ext-hash")]
            Evaluator::Rand(law) => self.eval_rand(law, node, input, cont, resources),
            // The IP/CIDR family: pure value laws over the piped string, with
            // `ip_in_cidr`'s CIDR filter argument evaluated over the same
            // input (the argument-evaluation law) before the law runs.
            #[cfg(feature = "ext-net")]
            Evaluator::Net(law) => self.eval_net(law, node, input, cont, resources),
            // The TOP-K partial sort: bounded-heap O(n log k) with optional
            // per-element projection filter.
            Evaluator::TopK(law) => self.eval_topk(law, node, input, cont, resources),
            // The PARSERS family: pure string-to-object laws over the piped
            // value. `parse_grok` evaluates its pattern filter argument over
            // the same input (the argument-evaluation law) before the law
            // runs; the unary parsers never touch their (empty) argument list.
            Evaluator::Parser(law) => self.eval_parser(law, node, input, cont, resources),
            // The JSON-Pointer family: navigate an RFC 6901 pointer string
            // over the piped value (read form) or over each value a source
            // filter yields, emitting one `[match]`/`[]` array per source.
            Evaluator::Pointer(law) => self.eval_pointer(law, node, input, cont, resources),
            // The JSONPath family: evaluate an RFC 9535 query string over the
            // piped value (read form) or over each value a source filter
            // yields, emitting one nodelist array per query per source.
            #[cfg(feature = "ext-jsonpath")]
            Evaluator::JsonPath(law) => self.eval_jsonpath(law, node, input, cont, resources),
            // The schema family: infer a JSON Schema 2020-12 document from the
            // evaluated VALUE argument, or validate an evaluated value against
            // an evaluated schema document (boolean or ordered error objects).
            #[cfg(feature = "ext-schema")]
            Evaluator::Schema(law) => self.eval_schema(law, node, input, cont, resources),
            // The user-declared reusable index (`declare_index/2`): a TRANSPARENT
            // acceleration declaration — build a sorted keyed index over a
            // located container and pass the input through unchanged. Every
            // decline is a no-op declaration, never a raise.
            Evaluator::IndexDeclare => self.eval_index_declare(node, input, cont, resources),
            Evaluator::Transpose => self.eval_owned_law(input, cont, resources, reshape_builtins::transpose),
            // `has` reads the input WITHOUT materializing it — the one real
            // claim behind the conservative `Subtree` transfer it declares.
            Evaluator::Has => self.eval_answer(AnswerForm::HasKey, node, input, cont, resources),
            Evaluator::Walk => self.eval_walk(node, input, cont, resources),
            Evaluator::MapValues => self.eval_map_values(node, input, cont, resources),
            // The generator family. Each opens a `Generate` frame and returns:
            // a source's values are produced by RESUMING that frame, never by
            // an eager loop here.
            Evaluator::Generate(generator) => self.eval_generate(generator, node, input, cont, resources),
        }
    }

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
        let (values, pending) = self.eval_owned_argument_prefix(resources, slots, filter, input)?;
        if let Some(error) = pending {
            return Err(error);
        }
        Ok(values)
    }

    pub(crate) fn eval_owned_argument_prefix(
        &mut self,
        resources: &mut ResourceContext<'_>,
        slots: &[Option<Value>],
        filter: ProgramNodeId,
        input: Value,
    ) -> Result<(Vec<Value>, Option<EngineRunError>), EngineRunError> {
        if argument_inline_eligible(self.nodes, filter) {
            return self.eval_owned_argument_inline(resources, filter, input);
        }
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
        result
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

    /// Enters one frozen subexpression position.
    pub(crate) fn path_enter_frozen(&mut self) {
        if let Some(register) = self.path.as_mut() {
            register.enter_frozen();
        }
    }

    /// Leaves one frozen position at its consumer (`exit` restores the
    /// snapshot; `abandon` — the raise walk or a cut popped the drive — only
    /// releases the freeze).
    pub(crate) fn path_exit_frozen(&mut self) {
        if let Some(register) = self.path.as_mut() {
            register.exit_frozen();
        }
    }

    pub(crate) fn path_abandon_frozen(&mut self) {
        if let Some(register) = self.path.as_mut() {
            register.abandon_frozen();
        }
    }

    /// Whether the register is currently frozen.
    pub(crate) fn path_frozen(&self) -> bool {
        self.path.as_ref().is_some_and(path_register::PathRegister::is_frozen)
    }

    /// The owned form of one value in flight (located materializes).
    pub(crate) fn path_owned(
        &mut self,
        value: &EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Value, EngineRunError> {
        match value {
            EngineResult::Owned(owned) => Ok(owned.clone()),
            EngineResult::Located(_) => {
                self.materialize_capture(value.try_clone().map_err(EngineRunError::Codec)?, resources)
            }
        }
    }

    /// Law-2 pre-check before a stage descent.
    pub(crate) fn path_check_navigation(
        &mut self,
        steps: &[StageStep],
        at: usize,
        value: &EngineResult<'source>,
        _cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        let Some(step) = steps.get(at) else {
            return Ok(());
        };
        if self.path_frozen() {
            return Ok(());
        }
        let tracked = self.path.as_ref().is_some_and(|register| register.tracks_result(value));
        if tracked {
            return Ok(());
        }
        let owned = self.path_owned(value, resources)?;
        // The untracked rejection (`.[]` = iterate class, else access class).
        match step.access() {
            StepAccess::Each => Err(jqf_builtins::semantics::path::invalid_iterate(&owned, resources)),
            StepAccess::Descend => Ok(()),
            _ => {
                let env_slots = self.path_env_slots(resources)?;
                let Some(accessor) = path_register::step_component_value(step, &env_slots, resources)? else {
                    return Ok(());
                };
                Err(jqf_builtins::semantics::path::invalid_access(
                    &accessor, &owned, resources,
                ))
            }
        }
    }

    /// The untracked rejection for one step over one value, with the step's
    /// rendered component as the accessor.
    pub(crate) fn path_reject_access(
        &mut self,
        step: &StageStep,
        value: &Value,
        resources: &mut ResourceContext<'_>,
    ) -> Result<EngineRunError, EngineRunError> {
        let env_slots = self.path_env_slots(resources)?;
        let accessor = path_register::step_component_value(step, &env_slots, resources)?
            .ok_or_else(|| EngineRunError::internal_contract("a dynamic step carried a non-component bound"))?;
        Ok(jqf_builtins::semantics::path::invalid_access(
            &accessor, value, resources,
        ))
    }

    /// Law-2 write half: extend the register by one stage's components (the
    /// leaf IS the register's new address by construction — the tracks check
    /// ran in [`Self::path_check_navigation`]).
    pub(crate) fn path_extend_navigation(
        &mut self,
        steps: &[StageStep],
        at: usize,
        leaf: &EngineResult<'source>,
        _cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        if self.path.is_none() || self.path_frozen() {
            return Ok(());
        }
        let at_node = leaf.located().map(LocatedProduct::node);
        let at_value = self.path_owned(leaf, resources)?;
        self.path_extend_components(steps.get(at..).unwrap_or(&[]), &at_value, at_node, resources)
    }

    /// FanOut-prefix write: the steps BEFORE `.[]` extend the register.
    pub(crate) fn path_extend_prefix(
        &mut self,
        steps: &[StageStep],
        at: usize,
        next_at: usize,
        container: &Container<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        if next_at <= at || self.path.is_none() || self.path_frozen() {
            return Ok(());
        }
        // A markup member step fans out through LocatedFiltered: the Key
        // step selected children by element name and is not itself a path
        // component. The document's real paths are the matched indices,
        // recorded per child.
        if matches!(container, Container::LocatedFiltered(..)) {
            return Ok(());
        }
        let (at_value, at_node) = match container {
            Container::Owned(owned) => (owned.clone(), None),
            Container::Located(located) | Container::LocatedFiltered(located, _) => {
                let at_node = Some(located.node());
                let handle = located.try_clone().map_err(EngineRunError::Codec)?;
                (
                    self.materialize_capture(EngineResult::Located(handle), resources)?,
                    at_node,
                )
            }
        };
        self.path_extend_components(&steps[at..next_at], &at_value, at_node, resources)
    }

    /// One tracked extension scoped by a [`GraphFrame::PathStep`] marker.
    pub(crate) fn path_extend_components(
        &mut self,
        prefix: &[StageStep],
        at_value: &Value,
        at_node: Option<jqf_data::NodeHandle>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        let env_slots = self.path_env_slots(resources)?;
        self.path_scope(resources, |register, resources| {
            let components = register.stage_components(prefix, 0, &env_slots, resources)?;
            for component in components {
                register.push_step(component, at_value.clone(), at_node)?;
            }
            Ok(())
        })
    }

    /// Each-child write: the child's component extends the register.
    pub(crate) fn path_each_child(
        &mut self,
        child: &EngineResult<'source>,
        component: Option<jqf_builtins::semantics::path::PathStep>,
        _cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        if self.path.is_none() || self.path_frozen() {
            return Ok(());
        }
        let Some(component) = component else {
            return Ok(());
        };
        let at_node = child.located().map(LocatedProduct::node);
        let at = self.path_owned(child, resources)?;
        self.path_scope(resources, |register, _| register.push_step(component, at, at_node))
    }

    /// The pre/post of one tracked extension, scoped by a `PathStep` marker.
    pub(crate) fn path_scope(
        &mut self,
        resources: &mut ResourceContext<'_>,
        extend: impl FnOnce(&mut path_register::PathRegister, &mut ResourceContext<'_>) -> Result<(), EngineRunError>,
    ) -> Result<(), EngineRunError> {
        let (steps_len, at, at_node) = {
            let Some(register) = self.path.as_mut() else {
                return Err(internal_contract("path_scope without its register"));
            };
            let pre = register.pre_navigation();
            extend(register, resources)?;
            pre
        };
        self.push_frame(GraphFrame::PathStep { steps_len, at, at_node });
        Ok(())
    }

    /// The owned form of one env slot (a located one materializes).
    pub(crate) fn path_slot_owned(
        &mut self,
        slot: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Value, EngineRunError> {
        match self.env.as_slice().get(slot) {
            Some(EngineResult::Owned(owned)) => Ok(owned.clone()),
            Some(EngineResult::Located(_)) => {
                self.path_owned(&self.env[slot].try_clone().map_err(EngineRunError::Codec)?, resources)
            }
            None => Err(internal_contract(
                "a variable stage read an env slot outside the sized env",
            )),
        }
    }
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
            let mut fresh = Box::new(GraphMachine::seed(
                self.nodes, node, node, 0, self.slots, self.scans, self.topk, self.anti, input,
            )?);
            fresh.iteration_cap = self.iteration_cap;
            fresh
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

    /// per-child key collection.
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
        *key = match previous {
            jqf_builtins::semantics::keyed::KeyValue::Empty => jqf_builtins::semantics::keyed::KeyValue::One(owned),
            jqf_builtins::semantics::keyed::KeyValue::One(first) => {
                let mut array = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
                array
                    .try_push(first)
                    .map_err(|_| EngineRunError::allocation_failure())?;
                array
                    .try_push(owned)
                    .map_err(|_| EngineRunError::allocation_failure())?;
                jqf_builtins::semantics::keyed::KeyValue::Many(array)
            }
            jqf_builtins::semantics::keyed::KeyValue::Many(mut array) => {
                array
                    .try_push(owned)
                    .map_err(|_| EngineRunError::allocation_failure())?;
                jqf_builtins::semantics::keyed::KeyValue::Many(array)
            }
            jqf_builtins::semantics::keyed::KeyValue::Bare(_) => {
                return Err(internal_contract(
                    "a bare key inside the keyed collect's per-child capture",
                ));
            }
        };
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
            KeyValue::Bare(_) => Err(internal_contract(
                "a bare key inside the streamed aggregate's per-element capture",
            )),
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
        let width = if let Container::Located(_) = &container {
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
        } else {
            match &container {
                Container::Owned(value) => match value.untagged() {
                    Value::Array(array) => array.len(),
                    Value::Object(object) => object.len(),
                    _ => unreachable!("owned container validated above"),
                },
                Container::Located(_) => unreachable!("located arm handled above"),
                Container::LocatedFiltered(..) => {
                    unreachable!("filtered markup containers never reach the keyed drive")
                }
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
        let mut values = Vec::new();
        let mut pending = None;
        'pointers: for path in &paths {
            let tokens = match pointer_builtins::parse_tokens(path, resources) {
                Ok(tokens) => tokens,
                Err(error) => {
                    pending = Some(error);
                    break;
                }
            };
            for source in sources {
                match pointer_builtins::match_at(source, &tokens, resources) {
                    Ok(array) => {
                        values
                            .try_reserve(1)
                            .map_err(|_| EngineRunError::allocation_failure())?;
                        values.push(array);
                    }
                    Err(error) => {
                        pending = Some(error);
                        break 'pointers;
                    }
                }
            }
        }
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
        let mut values = Vec::new();
        let mut pending = None;
        'queries: for query in &queries {
            let parsed = match jsonpath_builtins::parse_query(query, resources) {
                Ok(parsed) => parsed,
                Err(error) => {
                    pending = Some(error);
                    break;
                }
            };
            for source in sources {
                match jsonpath_builtins::match_query(&parsed, source, resources) {
                    Ok(array) => {
                        values
                            .try_reserve(1)
                            .map_err(|_| EngineRunError::allocation_failure())?;
                        values.push(array);
                    }
                    Err(error) => {
                        pending = Some(error);
                        break 'queries;
                    }
                }
            }
        }
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

        let root_for_law = &root;
        let keys_for_law = &projection_keys;
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
                top_k_builtins::top_k_law(law, root_for_law, count_value, keys_for_law, resources)
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
        let (node, path, role, kind, out_cont) = match self.frames.as_slice().get(index) {
            Some(GraphFrame::FactAssignWrite {
                node,
                path,
                role,
                kind,
                out_cont,
                ..
            }) => (*node, path.clone(), role.clone(), kind.clone(), *out_cont),
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
        let payload = GraphMachine::normalize_fact_payload(&payload, &role, resources)?;
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
        let _ = out_cont;
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
    pub(crate) fn normalize_fact_payload(
        payload: &Value,
        role: &str,
        _resources: &ResourceContext<'_>,
    ) -> Result<Value, EngineRunError> {
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
    /// from the correlated-join memo:
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
    ///   declines and the naive path emits everything (hazard 9's negated
    ///   form);
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
        let Some(located) = self.scan_container(row.container_stage(), input, resources)? else {
            return Ok(false);
        };
        let handle = located.node();
        let container = Container::Located(located);
        let Some(len) = located_child_count(&container)? else {
            return Ok(false);
        };
        if len == 0 {
            return Ok(false);
        }
        let join_slot = self
            .scans
            .len()
            .checked_add(slot)
            .ok_or_else(|| internal_contract("anti-join slot overflow"))?;
        if !self.prepare_join_slot(join_slot, row.key_stage(), handle, &container, len, resources)? {
            return Ok(false);
        }
        if self.join_slot_serves_user(join_slot) {
            USER_INDEX_PROBES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        let Some(key) = self.probe_value(row.probe(), resources) else {
            return Ok(false);
        };
        let cursor_and_end = {
            let index = self
                .slot_index(join_slot)
                .ok_or_else(|| internal_contract("anti-join index vanished after preparation"))?;
            equal_key_run(index.entries.as_slice(), &key)
        };
        let Some((cursor, end)) = cursor_and_end else {
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
        let Some(located) = self.scan_container(scan.source(), input, resources)? else {
            return Ok(ScanRoute::Naive);
        };
        let handle = located.node();
        let container = Container::Located(located);
        let Some(len) = located_child_count(&container)? else {
            return Ok(ScanRoute::Naive);
        };
        // Hazard: an EMPTY container runs the outer key expression ZERO times in
        // the naive form (`Binary` is right-outer PER INVOCATION, and there is
        // no invocation), so a probe here could surface an error the naive form
        // never reaches.
        if len == 0 {
            return Ok(ScanRoute::Naive);
        }
        if !self.prepare_join_slot(slot, scan.key_stage(), handle, &container, len, resources)? {
            return Ok(ScanRoute::Naive);
        }
        if self.join_slot_serves_user(slot) {
            USER_INDEX_PROBES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        // Hazard: the probe must be TOTAL and single-valued. It is evaluated ONCE
        // where the naive form evaluates it once per child; a raise declines so
        // the naive scan raises in its own position.
        let Some(key) = self.probe_value(scan.probe(), resources) else {
            return Ok(ScanRoute::Naive);
        };
        let cursor_and_end = {
            let index = self
                .slot_index(slot)
                .ok_or_else(|| internal_contract("join index vanished after preparation"))?;
            equal_key_run(index.entries.as_slice(), &key)
        };
        let Some((cursor, end)) = cursor_and_end else {
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
                let root_for_law = &root;
                let keys_for_law = &projection_keys;
                top_k_builtins::top_k_law(law, root_for_law, &count_value, keys_for_law, resources)?
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
                let entry = EngineResult::Located(located.try_clone().map_err(EngineRunError::Codec)?);
                let descended = {
                    let env = self.env.as_slice();
                    let mut scratch = StepScratch::new(&mut self.workspace, resources).with_deltas(&self.fact_deltas);
                    descend(prefix, 0, env, entry, 0, &mut scratch)
                };
                match descended {
                    Ok(Descended::Leaf(EngineResult::Located(located))) => Ok(Some(located)),
                    _ => Ok(None),
                }
            }
            StageStart::Variable(variable) => {
                let variable = *variable;
                let Some(EngineResult::Located(bound)) = self.env.as_slice().get(variable as usize) else {
                    return Ok(None);
                };
                let entry = EngineResult::Located(bound.try_clone().map_err(EngineRunError::Codec)?);
                let descended = {
                    let env = self.env.as_slice();
                    let mut scratch = StepScratch::new(&mut self.workspace, resources).with_deltas(&self.fact_deltas);
                    descend(prefix, 0, env, entry, 0, &mut scratch)
                };
                match descended {
                    Ok(Descended::Leaf(EngineResult::Located(located))) => Ok(Some(located)),
                    _ => Ok(None),
                }
            }
            StageStart::Literal(_) => Ok(None),
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

    /// Returns one owned builtin result as an owned engine result. A computed
    /// value is never the register's: the box makes the allocation identity
    /// reject it (`path(.a | length)` over `{"a":5}` rejects with
    /// `with result 5`).
    pub(crate) fn retain_owned(&self, value: Value) -> EngineResult<'source> {
        if self.path.is_some() {
            EngineResult::owned(path_register::box_computed(value))
        } else {
            EngineResult::owned(value)
        }
    }

    /// Descends a `Stage` node's steps from `at`, routing a leaf through
    /// `[0..cont]` and pushing an `Each` frame on a fan-out.
    pub(crate) fn descend_stage(
        &mut self,
        node: ProgramNodeId,
        value: EngineResult<'source>,
        at: usize,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let nodes = self.nodes;
        let ProgramNode::Stage { steps, .. } = &nodes[node.index()] else {
            return Err(internal_contract("graph machine descended a non-Stage node"));
        };
        // Law 2's check runs BEFORE the descent (`?` never suppresses it).
        // The rejection raises AT the value's routing position — the same
        // `raise_cont` convention the mismatch arm below uses — so an inner
        // `try`/`?` barrier sees it instead of the enclosing path barrier.
        if self.path.is_some()
            && let Err(error) = self.path_check_navigation(steps, at, &value, cont, resources)
        {
            self.raise_cont = cont;
            return Err(error);
        }
        // The stage's own mismatch (index/iterate) fails a value routing at `cont`.
        let descended = {
            let mut scratch = StepScratch::new(&mut self.workspace, resources).with_deltas(&self.fact_deltas);
            descend(steps, 0, self.env.as_slice(), value, at, &mut scratch)
        };
        self.absorb_descent(node, steps, at, descended, cont, resources)
    }

    /// Descends a `Variable`-start stage over whichever representation the
    /// slot holds, cloning only what it emits. A LOCATED slot takes the
    /// engine's NATIVE walk on a re-borrow (an Arc-cheap `try_clone`, copying
    /// no document bytes), so its leaves stay located; an OWNED slot walks a
    /// BORROW ([`descend_borrowed`]), cloning only the emitted leaf.
    pub(crate) fn descend_variable_stage(
        &mut self,
        node: ProgramNodeId,
        slot: VarSlot,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let nodes = self.nodes;
        let ProgramNode::Stage { steps, .. } = &nodes[node.index()] else {
            return Err(internal_contract("graph machine descended a non-Stage node"));
        };
        // The register's Law-2 pre-check over the BOUND value (a borrow of
        // the slot — a clone would break the allocation identity `$x.a` over
        // a tracked `$x` relies on).
        if self.path.is_some()
            && !self.path_frozen()
            && let Some(step) = steps.first()
            && !matches!(step.access(), StepAccess::Descend)
        {
            // A located slot whose node is the register's current address
            // tracks without rematerializing (the same Law-1 node identity
            // the stage descent uses). An owned slot uses allocation identity.
            let tracked = self
                .env
                .as_slice()
                .get(slot as usize)
                .is_some_and(|held| self.path.as_ref().is_some_and(|register| register.tracks_result(held)));
            if !tracked {
                let owned = self.path_slot_owned(slot as usize, resources)?;
                return match step.access() {
                    StepAccess::Each => Err(jqf_builtins::semantics::path::invalid_iterate(&owned, resources)),
                    _ => Err(self.path_reject_access(step, &owned, resources)?),
                };
            }
        }
        let env = self.env.as_slice();
        let descended = {
            let mut scratch = StepScratch::new(&mut self.workspace, resources).with_deltas(&self.fact_deltas);
            match env.get(slot as usize) {
                Some(EngineResult::Located(located)) => match located.try_clone() {
                    Ok(handle) => descend(steps, 0, env, EngineResult::Located(handle), 0, &mut scratch),
                    Err(error) => Err(EngineRunError::Codec(error)),
                },
                Some(EngineResult::Owned(root)) => descend_borrowed(steps, 0, env, root, 0, &mut scratch),
                None => Err(internal_contract(
                    "a variable stage read an env slot outside the sized env",
                )),
            }
        };
        self.absorb_descent(node, steps, 0, descended, cont, resources)
    }

    /// The env slots as owned values (located ones materialize).
    pub(crate) fn path_env_slots(
        &mut self,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Vec<Option<Value>>, EngineRunError> {
        let mut slots = Vec::new();
        for index in 0..self.env.len() {
            let owned = match self.env[index].try_clone().map_err(EngineRunError::Codec)? {
                EngineResult::Owned(owned) => owned,
                EngineResult::Located(located) => {
                    self.materialize_capture(EngineResult::Located(located), resources)?
                }
            };
            slots.push(Some(owned));
        }
        Ok(slots)
    }

    /// Turns one descent outcome into the machine's next transition: a leaf
    /// routes, a fan-out pushes its `Each` frame, a suppressed step skips, and a
    /// mismatch fails a value routing at `cont`.
    pub(crate) fn absorb_descent(
        &mut self,
        node: ProgramNodeId,
        steps: &'program [StageStep],
        at: usize,
        descended: Result<Descended<'source>, EngineRunError>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let descended = match descended {
            Ok(descended) => descended,
            Err(error) => {
                self.raise_cont = cont;
                return Err(error);
            }
        };
        match descended {
            Descended::Leaf(item) => {
                // The register's Law-2 write half: extend the register by the
                // stage's components, scoped by a [`GraphFrame::PathStep`].
                self.path_extend_navigation(steps, at, &item, cont, resources)?;
                self.route(item, cont, resources)
            }
            Descended::FanOut { next_at, container } => {
                // A `.[]` fan-out: the PREFIX steps navigate; each child's
                // component extends further via the Each frame.
                self.path_extend_prefix(steps, at, next_at, &container, resources)?;
                self.push_frame(GraphFrame::Each {
                    container,
                    cursor: 0,
                    stage: node,
                    next_at,
                    cont,
                });
                Ok(None)
            }
            // `..` over a container: the frame's children resume AT the descend
            // step (recursing), while the SELF emission continues past it in the
            // pending slot — which the machine drains before advancing any frame,
            // giving the pre-order self-first walk.
            Descended::Descend {
                next_at,
                container,
                value,
                at: continue_at,
            } => {
                // A `..` fan-out: the PREFIX steps (before the descend step)
                // navigate, extending the register to the walked value; each
                // child extends further via the Each frame's emissions, and
                // the self-emission continues with the register already
                // parked on it.
                if self.path.is_some() && !self.path_frozen() {
                    self.path_extend_prefix(steps, at, next_at, &container, resources)?;
                }
                self.push_frame(GraphFrame::Each {
                    container,
                    cursor: 0,
                    stage: node,
                    next_at,
                    cont,
                });
                self.pending = Some(GraphTask::Descend {
                    node,
                    value,
                    at: continue_at,
                    cont,
                });
                Ok(None)
            }
            Descended::Skip => Ok(None),
        }
    }
}
