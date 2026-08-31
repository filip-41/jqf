//! Streaming slurp-aggregate rewrite: `map(F)|add` → fold over `inputs`.
//!
//! Recognition and rewrite live here, next to the other arena rewrites, so
//! the CLI no longer parses and rewrites the user's source text. The public
//! method stays on [`CompiledProgram`] because the host asks the compiled
//! program for the streaming equivalent.

use super::super::prelude::*;
use super::marks::mark_keyed_collects;
use super::push_rewrite_node;
use crate::compile::CompiledProgram;

impl CompiledProgram {
    /// Recognizes the slurp shape `map(F) | add` / `map(F) | length` on the
    /// compiled arena and returns the equivalent STREAMING fold
    /// (`reduce inputs as $x (INIT; . + ([($x | F)] | AGG))`), built directly
    /// in the arena.
    ///
    /// This was the CLI optimizer's source-text rewrite: `-s 'map(F) | add'`
    /// collected every record into the slurp array first — O(records) of
    /// allocator commit — where the streamed equivalent folds one record at a
    /// time through the `inputs` cursor and is byte-identical (`map` is
    /// `[.[] | F]`; an empty F contribution adds 0).
    ///
    /// On the arena `map` lowers to `[.[] | F]`, so the shape is a `FlatMap`
    /// whose upstream is a `CollectArray` over the `.[] | F` body and whose
    /// body is an arity-0 `add`/`length` call. The map body is reused with
    /// its leading `.[]` step consumed and the entry re-seated on the fold's
    /// `$x` slot, so the per-record contribution `[($x | F)]` is the same
    /// graph the collect would have run per element. Returns `None` for every
    /// other program.
    #[expect(
        clippy::too_many_lines,
        reason = "one linear arena build: recognize, transform the map body, assemble the fold; splitting it would thread the borrowed arena through helpers that each read one piece"
    )]
    #[must_use]
    pub fn streaming_slurp_aggregate(&self) -> Option<CompiledProgram> {
        let nodes = self.program.nodes();
        let root = self.program.root();
        let (map_body, suffix, is_length) = match &nodes[root.index()] {
            ProgramNode::FlatMap { upstream, body } => {
                let ProgramNode::Call { overload, args, .. } = &nodes[body.index()] else {
                    return None;
                };
                let is_length = overload.get() == jqf_builtins::registry::builtins::id::LENGTH;
                if !args.is_empty() || (overload.get() != jqf_builtins::registry::builtins::id::ADD && !is_length) {
                    return None;
                }
                let ProgramNode::CollectArray {
                    body: Some(collect_body),
                } = &nodes[upstream.index()]
                else {
                    return None;
                };
                (*collect_body, Some(*body), is_length)
            }
            // `[STREAM] | add` is rewritten to a reduce at compile; slurp still
            // needs the map-body `.[]` reseated onto the inputs `$x`.
            ProgramNode::Reduce { source, .. } => (*source, None, false),
            _ => return None,
        };
        // The collect body must be the `.[] | F` map shape: a fused stage
        // whose leading step is a plain `.[]`, or a `FlatMap` over exactly
        // that stage. The rewrite consumes the `.[]` and re-seats the entry
        // on the fold's `$x` slot.
        let slot = self.program.slots();
        let mut new_nodes = nodes.to_vec();
        let f_graph = match &nodes[map_body.index()] {
            ProgramNode::Stage {
                start: StageStart::Current,
                steps,
            } => {
                let [first, rest @ ..] = steps.as_slice() else {
                    return None;
                };
                if !matches!(first.access(), StepAccess::Each) || first.cell_suppressed() {
                    return None;
                }
                push_rewrite_node(
                    &mut new_nodes,
                    ProgramNode::Stage {
                        start: StageStart::Variable(slot),
                        steps: rest.to_vec(),
                    },
                )?
            }
            ProgramNode::FlatMap { upstream, body } => {
                let ProgramNode::Stage {
                    start: StageStart::Current,
                    steps,
                } = &nodes[upstream.index()]
                else {
                    return None;
                };
                let [first] = steps.as_slice() else {
                    return None;
                };
                if !matches!(first.access(), StepAccess::Each) || first.cell_suppressed() {
                    return None;
                }
                let x_stage = push_rewrite_node(
                    &mut new_nodes,
                    ProgramNode::Stage {
                        start: StageStart::Variable(slot),
                        steps: Vec::new(),
                    },
                )?;
                push_rewrite_node(
                    &mut new_nodes,
                    ProgramNode::FlatMap {
                        upstream: x_stage,
                        body: *body,
                    },
                )?
            }
            _ => return None,
        };
        let collect = push_rewrite_node(&mut new_nodes, ProgramNode::CollectArray { body: Some(f_graph) })?;
        let current = push_rewrite_node(
            &mut new_nodes,
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: Vec::new(),
            },
        )?;
        // The suffix call (add/length) is the ROOT's own call node, reused
        // with its pinned overload id and revision. A compile-time
        // `[STREAM]|add` reduce has no suffix call: the per-record
        // contribution is `f_graph` itself.
        let suffix_flat = match suffix {
            Some(suffix) => push_rewrite_node(
                &mut new_nodes,
                ProgramNode::FlatMap {
                    upstream: collect,
                    body: suffix,
                },
            )?,
            None => f_graph,
        };
        let update = push_rewrite_node(
            &mut new_nodes,
            ProgramNode::Binary {
                op: BinaryKind::Add,
                left: current,
                right: suffix_flat,
                shape: crate::program::BinaryShape::Framed,
            },
        )?;
        // `inputs` is the record cursor of the null-first drive the caller
        // runs the fold under, exactly as the CLI's rewritten source named it.
        let inputs = resolve_builtin("inputs", 0)?;
        let inputs_call = push_rewrite_node(
            &mut new_nodes,
            ProgramNode::call(inputs.id, inputs.semantic_revision, Vec::new()),
        )?;
        // `null` is the `+` identity for both numbers and strings (the add
        // aggregate's init); the length aggregate's init is the exact integer
        // zero.
        let init_value = if is_length {
            Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(0)))
        } else {
            Value::Null
        };
        let init = push_rewrite_node(
            &mut new_nodes,
            ProgramNode::Stage {
                start: StageStart::Literal(init_value),
                steps: Vec::new(),
            },
        )?;
        let reduce = push_rewrite_node(
            &mut new_nodes,
            ProgramNode::Reduce {
                source: inputs_call,
                slot,
                init,
                update,
                keyed_collect: None,
            },
        )?;
        mark_keyed_collects(&mut new_nodes);
        let split = analyze(&new_nodes, reduce);
        let program = Program::new(new_nodes, reduce, split, slot + 1);
        // The rewrite's own epilogue differences live in `finish`'s doc: the
        // prune gate this path used to carry was a no-op (the fold's
        // `inputs` source pull declines the root tree inside the analysis),
        // and the marks stay here because these nodes are hand-built with
        // explicit shape fields.
        Some(CompiledProgram::finish(program, self.policy, false, None))
    }
}
