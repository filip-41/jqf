//! The one iterative engine executor and the typed run entry it drives.
//!
//! One job: turn one interpreted codec input into an ordered engine result
//! stream, driving the residual graph — the steps from the first `.[]` on, plus
//! the whole downstream `Choice`/`FlatMap` graph — with explicit continuation
//! state and no Rust recursion in evaluation. Values in flight are
//! [`EngineResult`]: located handles flow untouched (a child is a fresh located
//! product into the same document), owned values appear only where already owned
//! (`null`). Every advance charges the shared work ledger and honors the same
//! cooperative pauses the codec side uses, so N executors can run with zero
//! shared mutable state.
//!
//! **Two machines behind one [`EngineRunStream`].** A bare-`Stage` residual (the
//! pure path/iteration subset — every pre-comma program) takes the landed
//! single-slot fast path STRUCTURALLY: [`EngineRunStream::seed`] slices the
//! stage's residual steps and drives them with the [`StageMachine`], which never
//! engages graph dispatch. A `Choice`/`FlatMap` residual engages the
//! [`GraphMachine`], which walks the arena.
//!
//! **Fast path ([`StageMachine`]).** The current task lives in a single slot; the
//! [`Vec`] `.[]` frame stack allocates LAZILY on the first
//! descent. Identity and pure-path residuals never touch it, so the
//! `identity_run_leaves_working_memory_untouched` pin stays green.
//!
//! **Graph path ([`GraphMachine`]).** One unified [`GraphFrame`] stack, one pending
//! slot, `(node, step_offset)` continuations. `Each` frames fan a container out;
//! `Choice` frames re-seed the right member over the same re-borrowed input
//! (fallible [`EngineResult::try_clone`] at the fork); `FlatMapBody` frames are
//! the consumers. The control-flow families add three consumer frames: a
//! `ConditionalBody` selects an arm per condition output, a `LogicalLeft`/
//! `LogicalRight` pair drives the LEFT-outer short-circuiting `and`/`or` and
//! booleanizes right outputs, and an `AlternativeLeft` is a FILTERING PASS-THROUGH
//! (truthy left outputs re-emitted upward, falsy swallowed, the `//` fallback
//! seeded only on left-complete-with-zero-truthy). **Emission-routing law:** a
//! produced leaf walks the live frame prefix from the top; the first CONSUMER in
//! range consumes it (a `FlatMapBody` runs its body over the item, depth-first,
//! before the upstream resumes; a control-flow consumer applies its selection or
//! filtering law); `Each`/`Choice` frames pass emissions through; none in range →
//! an ordered output item.
//!
//! Negative space: it does not parse, lower, analyze, or build codec
//! requirements, and it collects nothing — one value is in flight per frame and
//! nothing buffers a fan-out's children. Per-step navigation lives in [`stage`]
//! (field/index walking, iterate classification, one-child materialization); the
//! drivers here own cardinality and graph continuation. Emitted-prefix-then-error
//! is an ordered-stream property: element k's failure surfaces on its own poll
//! advance as an `Err`, after items 1..k-1 were emitted on earlier advances — no
//! transition both emits and fails. An unsuppressed failure discards the entire
//! frame stack (the stream is done; no outer cursor survives).
//! [`StepOutcome::EmitThenFail`] remains the vocabulary for a genuinely atomic
//! emit-then-fail transition and keeps its producerless unit test.
//!
//! Sibling layout: [`frames`] owns graph-machine types and helpers,
//! [`stage_machine`] is the bare-Stage fast path, [`eval`] seeds and
//! dispatches, [`fold`] owns bind/countdown/reduce, [`pathmode`] hands off to
//! the path register, [`dispatch`] maps a resolved call onto the family
//! evaluator, [`route`] is the emission-routing and break-unwind seam,
//! [`oracles`] answers the committed document shortcut, and [`stream`] is the
//! public poll facade.

// The error vocabulary lives in `jqf-builtins`; exec re-exports it so the
// machine's call sites keep the `crate::exec::` spelling. The host seams
// (input source / module loader) live there too.
pub(crate) use jqf_builtins::error::message;
pub(crate) use jqf_builtins::error::mismatch;
pub(crate) use jqf_builtins::error::{ArithFailure, EngineRunError};
pub use jqf_builtins::host::{
    InputSource, InputSourceError, InputSourceHandle, LoadedModule, ModuleLoader, ModuleLoaderHandle, with_input_source,
};
pub(crate) use jqf_builtins::semantics::{accessor_matches_fact, materialize_fact_payload};

pub(crate) mod path_register;
pub(crate) mod stage;

mod prelude;
pub(crate) use prelude::*;

mod frames;
pub(crate) use frames::*;

mod stage_machine;
pub(crate) use stage_machine::*;

mod dispatch;
mod eval;
mod fold;
mod pathmode;
mod route;
mod stream;
pub use frames::{FactDelta, RunPoll};
pub(crate) use stream::try_run_program;
#[cfg(test)]
pub(crate) use stream::try_run_with_table;
pub use stream::{EngineRun, EngineRunStream, RunInput};

mod oracles;

#[cfg(test)]
mod tests;
