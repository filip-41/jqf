//! Whether a program's root evaluation consumes the entire input document.
//!
//! This is the route decision behind the span-backed lazy representation: it
//! pays extra time on touch-everything programs — identity, `..`,
//! root-element folds, whole-document `|=` — and can even raise their peak
//! RSS (the eager document is replaced by spans, but publication materializes
//! every one of them), so those programs must stay on the eager decode. The
//! programs this analysis classifies partial — root-structure reads like
//! `length` at the root and member-scoped chains — are faster and lighter
//! under spans. Folds whose generator iterates a container
//! (`reduce .a[] as …`) classify as consuming: the fold visits every element
//! the generator yields, and when the iterated member is the document's bulk
//! the lazy route pays double. A ROOT-STAGE fan-out behind a static prefix
//! (`.a[]`) is the same shape without the fold, and classifies consuming for
//! the same reason.
//!
//! Conservative by construction, in the SAFE direction: a program this
//! analysis cannot PROVE partial is classified consuming (eager). The
//! misclassification that costs time and RSS is the one that sends a
//! touch-everything program to the lazy route; over-reporting only loses a
//! flat-or-better lazy win.
//!
//! The context rules mirror `reads_outer_dot` (demand.rs): a `FlatMap` body
//! sees the upstream outputs, a `Reduce`/`Foreach` update/extract sees the
//! fold state, a `Try` handler sees the caught value, and a `Stage` whose
//! start is a bound variable or a literal ignores its input entirely. Only
//! the source and init of a loop, and the source and body of a binder, run
//! over the outer dot.

use crate::program::{ProgramNode, ProgramNodeId, StageStart, StageStep, StepAccess};
use jqf_builtins::registry::builtins::id;

/// Whether evaluating `id` over the ROOT INPUT reads every node of it.
///
/// `has_indexed_scans` is the program's precomputed correlated-scan and
/// anti-join emptiness (`Program::scans` / `Program::anti_joins`, both built
/// once at construction): passing it here keeps this classification from
/// re-walking the arena for tables it could read off the `Program`.
pub(crate) fn consumes_whole_document(nodes: &[ProgramNode], id: ProgramNodeId, has_indexed_scans: bool) -> bool {
    // A program with a recognized correlated scan or anti-join
    // touches whole containers, and the sorted keyed index's BUILD needs the
    // eager decode: a deferred container span makes the build decline and
    // returns the scan to the naive Θ(k·m). The scan's FlatMap sits inside a
    // FlatMap BODY (which the classifier deliberately does not walk — the
    // body sees the upstream's outputs), so the table's non-emptiness is the
    // reachable fact. Classing the whole program consuming forfeits only the
    // lazy skip of whatever lies outside the iterated containers — the
    // fold rule's own asymmetry, and the safe direction.
    if has_indexed_scans {
        return true;
    }
    consumes_whole_document_impl(nodes, id)
}

#[expect(
    clippy::match_same_arms,
    reason = "one arm per ProgramNode kind: the table is exhaustive by node vocabulary, not by body, so a new node kind forces its own classification decision and each arm keeps its own reason"
)]
fn consumes_whole_document_impl(nodes: &[ProgramNode], id: ProgramNodeId) -> bool {
    match &nodes[id.index()] {
        ProgramNode::Stage { start, steps } => match start {
            StageStart::Current => {
                steps.is_empty()
                    // A LEADING root iteration fans every element out; a
                    // leading recursive descent walks every node.
                    || matches!(
                        steps.first().map(StageStep::access),
                        Some(StepAccess::Each | StepAccess::Descend)
                    )
                    // A fan-out behind a STATIC prefix iterates the container
                    // the prefix reached from the ROOT (`.a[]`, `.a.b[]`,
                    // `.a[0][]`): when that container is the document's bulk,
                    // the lazy route pays the touch-everything penalty — the
                    // same asymmetry the fold rule names, one hop earlier.
                    // The scan stops at the first non-static step: an Each or
                    // Descend there consumes (it is this case), anything else
                    // (`.[$k]`, a slice) leaves the answer to the rules above,
                    // and what lies past it operates on subtrees the root
                    // rule cannot name anyway.
                    || static_prefix_fans_out(steps)
            }
            // A literal or bound-variable producer ignores its input entirely.
            StageStart::Literal(_) | StageStart::Variable(_) => false,
        },
        // Only the UPSTREAM sees the root input; the body sees its outputs.
        ProgramNode::FlatMap { upstream, .. } => consumes_whole_document_impl(nodes, *upstream),
        ProgramNode::Choice { left, right }
        | ProgramNode::Binary { left, right, .. }
        | ProgramNode::Alternative { left, right }
        | ProgramNode::Logical { left, right, .. } => {
            consumes_whole_document_impl(nodes, *left) || consumes_whole_document_impl(nodes, *right)
        }
        ProgramNode::Concat { parts } => parts.iter().any(|part| consumes_whole_document_impl(nodes, *part)),
        ProgramNode::CollectArray { body } => body.is_some_and(|body| consumes_whole_document_impl(nodes, body)),
        // The count-only collect: `[f] | length` WITHOUT the array. The
        // body still runs per element (its outputs are counted, not built), so
        // a whole-document body consumes exactly as the collect's would.
        ProgramNode::CountCollect { body } => body.is_some_and(|body| consumes_whole_document_impl(nodes, body)),
        ProgramNode::ConstructObject { members } => members.iter().any(|member| {
            consumes_whole_document_impl(nodes, member.key) || consumes_whole_document_impl(nodes, member.value)
        }),
        ProgramNode::Call {
            overload,
            args,
            revision: _,
            payload: _,
        } => {
            call_consumes_whole_input(overload.get())
                || args.iter().any(|arg| consumes_whole_document_impl(nodes, *arg))
        }
        // A recursive definition may walk anything; conservative.
        ProgramNode::CallDef { .. } => true,
        // A filter-parameter use evaluates the captured argument over the
        // current input; conservative.
        ProgramNode::CallFilter { .. } => true,
        // An assignment folds the whole document it rewrites, whatever its
        // path and update graphs read.
        ProgramNode::Modify { .. } => true,
        // A fact assignment resolves its path over the whole located input and
        // runs its update per node, so it consumes the document too.
        ProgramNode::FactAssign { .. } => true,
        ProgramNode::Conditional {
            condition,
            consequent,
            alternative,
        } => {
            consumes_whole_document_impl(nodes, *condition)
                || consumes_whole_document_impl(nodes, *consequent)
                || consumes_whole_document_impl(nodes, *alternative)
        }
        // The handler sees the caught value, not the root; still walked,
        // which can only classify more conservatively.
        ProgramNode::Try { body, handler } => {
            consumes_whole_document_impl(nodes, *body)
                || handler.is_some_and(|handler| consumes_whole_document_impl(nodes, handler))
        }
        ProgramNode::ChainBody { body } => consumes_whole_document_impl(nodes, *body),
        ProgramNode::Empty => false,
        ProgramNode::Label { body, .. } => consumes_whole_document_impl(nodes, *body),
        ProgramNode::Break { .. } => false,
        // A binder's source AND body both run over the outer dot.
        ProgramNode::Bind { source, body, .. } => {
            consumes_whole_document_impl(nodes, *source) || consumes_whole_document_impl(nodes, *body)
        }
        // A loop's source and init run over the outer dot; update/extract run
        // with dot = the fold state, so they never consume the root document
        // (a `Modify` inside an update rewrites the STATE, not the input).
        // A fold additionally VISITS every element its generator yields, so a
        // container-iterating source (`.a[]`, `.a | ..`) makes the fold touch
        // everything under that container even though the stage rule alone
        // calls a non-leading iteration partial. When the iterated member IS
        // the document — `{"catalog": [...]}`, the commonest real shape — the
        // lazy route pays the touch-everything penalty at double
        // (+100 % on `[foreach .catalog[] as $x (0; . + 1)] | length`).
        // Classing these consuming forfeits only the lazy skip of whatever
        // lies OUTSIDE the iterated container — the safe direction under this
        // module's asymmetry.
        ProgramNode::Reduce { source, init, .. } | ProgramNode::Foreach { source, init, .. } => {
            consumes_whole_document_impl(nodes, *source)
                || consumes_whole_document_impl(nodes, *init)
                || iterates_input_container(nodes, *source)
        }
        ProgramNode::Counted { source, .. } => consumes_whole_document_impl(nodes, *source),
        // An engine binding's body runs over the outer dot and may pull the
        // cursor; a pull is a STATE-CARRYING side effect the lazy skip cannot
        // reason about; the cursor body's filters run over the cursor state.
        // Classing all three consuming is the safe direction (it only forfeits
        // the lazy skip).
        ProgramNode::EngineBind { .. }
        | ProgramNode::EnginePull { .. }
        | ProgramNode::EngineGenerator { .. }
        | ProgramNode::EngineRng { .. } => true,
    }
}

/// Whether a stage's step list fans the ROOT input out behind a static
/// prefix: every step before the first Each/Descend is a Key or an Index.
///
/// `.a[]`, `.a.b[]` and `.a[0][]` all answer true — the iterated container is
/// reached from the root by static navigation, so its elements are (a subset
/// of) the root document's own nodes. A non-static step before the iteration
/// (`.[].a[]`'s inner hop, a dynamic index, a slice) stops the scan and
/// answers false; the miss only leaves a program on the lazy route it already
/// rode, which is this module's safe direction.
fn static_prefix_fans_out(steps: &[StageStep]) -> bool {
    for step in steps {
        match step.access() {
            StepAccess::Key(_) | StepAccess::Index(_) => {}
            StepAccess::Each | StepAccess::Descend => return true,
            _ => return false,
        }
    }
    false
}

/// Whether a fold GENERATOR fans out the elements of a container reached
/// from its input — `.a[]`, `.a[].b`, `.a | ..`, `(.a[], .b[])`.
///
/// Only shapes that PROVABLY iterate document structure answer true: a stage
/// must start from the input (a literal or bound-variable start never touches
/// the document, so `range(3)` and `$xs[]` stay partial) and contain an
/// iteration or descent step. Everything unrecognized answers false — this
/// helper widens the fold rule in `consumes_whole_document`, and a miss there
/// only leaves a program on the lazy route it already rode.
fn iterates_input_container(nodes: &[ProgramNode], id: ProgramNodeId) -> bool {
    match &nodes[id.index()] {
        ProgramNode::Stage { start, steps } => match start {
            StageStart::Current => steps
                .iter()
                .any(|step| matches!(step.access(), StepAccess::Each | StepAccess::Descend)),
            // A literal or bound-variable producer never touches the document,
            // so no container of it can be iterated.
            StageStart::Literal(_) | StageStart::Variable(_) => false,
        },
        // `.a | .b[]`: the body iterates a container the upstream reached.
        ProgramNode::FlatMap { upstream, body } => {
            iterates_input_container(nodes, *upstream) || iterates_input_container(nodes, *body)
        }
        ProgramNode::Choice { left, right } | ProgramNode::Alternative { left, right } => {
            iterates_input_container(nodes, *left) || iterates_input_container(nodes, *right)
        }
        ProgramNode::ChainBody { body } | ProgramNode::Label { body, .. } => iterates_input_container(nodes, *body),
        // A counted stream (`limit(3; .a[])`) iterates whatever its source
        // iterates — bounded, but still up to the count, which on a bulk
        // container is the double-pay shape.
        ProgramNode::Counted { source, .. } => iterates_input_container(nodes, *source),
        // A binder's source AND body both run over the outer dot (the
        // `limit`/`skip` expansions bind their count first), so both are
        // walked like generator positions.
        ProgramNode::Bind { source, body, .. } => {
            iterates_input_container(nodes, *source) || iterates_input_container(nodes, *body)
        }
        // A call's argument graphs run over the call's own input
        // (`reduce (_f(.a[])) as …`), so they are walked like any other
        // generator position. The call's OUTPUT is not walked — unknown
        // builtins stay false here, which is the safe direction.
        ProgramNode::Call { args, .. } => args.iter().any(|arg| iterates_input_container(nodes, *arg)),
        _ => false,
    }
}

/// Whether a resolved builtin consumes its whole input value when called at
/// the root.
///
/// The closed table of SHALLOW probes — calls that read only the container's
/// own structure (`length`, `type`, `keys`), answer a predicate about it
/// (`has`, `select`'s own drive), or raise/halt without touching it. Every
/// other builtin (`tostring`, `add`, `sort`, `paths`, `join`, `min`/`max`,
/// `recurse`, the kind filters excluded) materializes or walks its input, so
/// it is conservative-consuming. `select`'s ARGUMENT is analyzed separately:
/// it is a filter over the call's input, so `select(..)` consumes while
/// `select(.a)` does not.
fn call_consumes_whole_input(overload: u16) -> bool {
    !matches!(
        overload,
        id::LENGTH
            | id::KEYS
            | id::KEYS_UNSORTED
            | id::SELECT
            | id::NOT
            | id::TYPE
            | id::TAG
            | id::HAS
            | id::ERROR
            | id::ERROR_ONE
            | id::HALT
            | id::HALT_ERROR_0
            | id::HALT_ERROR_1
            | id::BOOLEANS
            | id::NUMBERS
            | id::STRINGS
            | id::ARRAYS
            | id::OBJECTS
            | id::ITERABLES
            | id::SCALARS
            // `sample(n)` reads a BOUNDED random subset of the array — the
            // elements are addressable by index, not all touched — so a
            // whole-array program whose root is a sample call is NOT a
            // touch-everything consumer: it gets the lazy container spans,
            // which run faster on a bounded sample lane. `shuffle`/`fill_forward`
            // genuinely need every element and stay conservative-consuming.
            | id::SAMPLE_1
    )
}

#[cfg(test)]
mod tests {
    use super::consumes_whole_document;
    use crate::codec_requirement::CodecRequirementPolicy;
    use crate::compile::try_compile_program;
    use jqf_codec_core::{DiagnosticPolicy, ValidationMode};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("resources")
    }

    fn consumes(program: &str) -> bool {
        let resources = resources();
        let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
        let compiled = try_compile_program(program, policy, &resources).expect("probe program compiles");
        // The same precomputed-emptiness derivation the production caller
        // reads off the `Program`.
        let has_indexed_scans = !crate::analysis::correlated_scans(compiled.arena()).is_empty()
            || !crate::analysis::anti_joins(compiled.arena()).is_empty();
        consumes_whole_document(compiled.arena(), compiled.root(), has_indexed_scans)
    }

    #[test]
    fn touch_everything_programs_consume() {
        for program in [
            ".", // identity
            "..",
            ".. | length",
            "[..] | length",
            "[.[]] | length",
            "reduce .. as $x (0; . + 1)",
            "reduce .[] as $x (0; . + $x)",
            // Folds over a container-iterating generator: the fold visits
            // every element the generator yields.
            "reduce .a[] as $x (0; . + $x)",
            "[foreach .catalog[] as $x (0; . + 1)] | length",
            "foreach (.a | .b[]) as $x (0; . + 1)",
            "reduce (.a | ..) as $x (0; . + 1)",
            // A root fan-out behind a STATIC prefix iterates the container the
            // prefix reached — the fold rule's shape without the fold.
            ".a[]",
            ".a.b[]",
            ".a[0][]",
            ".a[] | .name",
            ".a | ..",
            "[.a[] | select(.active) | .name] | length",
            "map(.) | length",
            "[paths] | length",
            ". |= .",
            ".[] | .name",
            "(., .a)",
            "if .a then .. else . end",
            "select(..)",
        ] {
            assert!(consumes(program), "{program} must consume");
        }
    }

    #[test]
    fn wrapped_generators_inside_folds_consume() {
        // A counted stream over an iterating source: bounded, but still every
        // element up to the count.
        assert!(consumes("reduce first(.a[]) as $x (0; . + $x)"));
        assert!(consumes("foreach (limit(3; .a[])) as $x (0; . + 1)"));
    }

    #[test]
    fn partial_programs_do_not_consume() {
        for program in [
            "length",
            "type",
            "keys",
            ".a",
            ".a | .b",
            "[.a] | length",
            // A fold whose generator yields ONE value visits no container.
            "reduce .a as $x (0; . + $x)",
            // A slice before the iteration stops the static-prefix scan: the
            // fan-out is not provably root-reached by static navigation.
            ".a[1:3] | length",
            "select(.active)",
            ".a[0]",
            "1",
            "empty",
        ] {
            assert!(!consumes(program), "{program} must not consume");
        }
    }

    #[test]
    fn the_correlated_join_shapes_consume() {
        // The correlated-scan and anti-join FlatMaps iterate whole
        // containers, and the sorted index's build needs the eager decode — a
        // deferred span makes it decline and returns the scan to Θ(k·m). The
        // recognized shapes class consuming even though a plain non-leading
        // iteration (`.a[] | .name`) stays partial.
        for program in [
            // The equi-join spelling.
            ".o as $o | [.u[] | . as $x | [$o[] | select(.k == $x.k)] | length]",
            // The anti-join spelling.
            ".o as $o | [.u[] | . as $x | select(all($o[]; .k != $x.k)) | .k]",
            // The benchmark's slice form.
            ".o as $o | [.u[0:3][] | . as $x | select(all($o[]; .k != $x.k)) | .k]",
        ] {
            assert!(consumes(program), "{program} must consume");
        }
    }
}
