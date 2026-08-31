//! Binary operators, concat, and framed two-sided control operators.
//!
//! One job: evaluate `Binary`, `Concat`, `Conditional`, `Alternative`, and
//! `Logical` over a graph-machine input. Call, keyed, builtins, generate,
//! modify, and join live in sibling files; [`super`] owns call dispatch.

#[allow(clippy::wildcard_imports)]
use super::super::*;

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

impl<'source> GraphMachine<'_, 'source> {
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
        // Shape is a compile-time fact (mark_post_fuse_facts after fuse).
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
}
