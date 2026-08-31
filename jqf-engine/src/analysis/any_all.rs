//! The `any`/`all` compile shortcut: inlined isempty/first spine plus an
//! admitted predicate, answered by an element visit.
//!
//! `any`/`all` are prelude defs, not `Call`s. After inline the root is the
//! `isempty(g) := first((g|false), true)` Label, optionally piped into `not`
//! (`any`) and optionally prefixed by a static PATH. The generator must be
//! `Stage{Current, [static Key/Index…, Each]}` plus `Logical{And|Or, P, empty}`
//! where `P` is an [`super::count::admitted_predicate`]. A Variable-start
//! generator is the anti-join row and is declined here.
//!
//! A pipe prefix and a generator path cannot both be nonempty: Exact locates
//! only the pipe prefix, so `.a | all(.b[]; .k)` is declined. `PATH | all(.k)`
//! (empty generator path) and `all(PATH[]; .k)` (no pipe prefix) stay rows.
//!
//! Polarity is exact: `And` without `not` is `all`; `Or` with outer `not` is
//! `any`. Identity `any`/`all` (condition `.`) is not an admitted predicate.
//! A raise before the first decisive item Declines so the graph renders it.
//! A decisive item (`any` truthy / `all` falsey) stops; remaining raises are
//! unobserved, matching the graph.

use alloc::vec::Vec;

use jqf_data::{CountFilter, CountStep};

use super::count::admitted_predicate;
use super::join::isempty_first_generator;
use super::path_steps::{static_member_prefix, static_path};
use crate::program::{LogicalOp, ProgramNode, ProgramNodeId, StageStart, StepAccess};
use jqf_builtins::registry::Evaluator;

/// Whether the boolean is `any` (some item matches) or `all` (every item matches).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AnyAllPolarity {
    /// `any(generator; condition)` — empty is false.
    Any,
    /// `all(generator; condition)` — empty is true.
    All,
}

/// A recognized `any`/`all` shortcut: the container path, the per-item filter,
/// and the polarity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AnyAllDemand {
    /// Static Key/Index steps to the iterated container. Empty is the document root.
    pub path: Vec<CountStep>,
    /// The admitted per-item predicate.
    pub filter: CountFilter,
    /// `any` vs `all`.
    pub polarity: AnyAllPolarity,
}

/// The recognized `any`/`all` shortcut, or `None` when the program is not a row.
pub(crate) fn any_all_demand(nodes: &[ProgramNode], root: ProgramNodeId) -> Option<AnyAllDemand> {
    if let ProgramNode::FlatMap { upstream, body } = &nodes[root.index()]
        && let Some(prefix) = static_member_prefix(nodes, *upstream)
    {
        let mut inner = match_expansion(nodes, *body)?;
        // Exact locates the pipe prefix only (the Label residual is not a
        // stage). Concatenating a generator path (`.a | all(.b[]; .k)`) would
        // iterate the located `.a` as if it were `.a.b`. Decline; the graph
        // still raises. PATH | all(.k) keeps an empty generator path.
        if !inner.path.is_empty() {
            return None;
        }
        inner.path = prefix;
        return Some(inner);
    }
    match_expansion(nodes, root)
}

fn match_expansion(nodes: &[ProgramNode], id: ProgramNodeId) -> Option<AnyAllDemand> {
    let (negated, inner) = peel_not(nodes, id);
    let generator = isempty_first_generator(nodes, inner)?;
    let ProgramNode::FlatMap {
        upstream: container,
        body: generator_body,
    } = &nodes[generator.index()]
    else {
        return None;
    };
    let path = iterating_current_path(nodes, *container)?;
    let ProgramNode::Logical {
        operator,
        left: cond,
        right: empty,
    } = &nodes[generator_body.index()]
    else {
        return None;
    };
    if !matches!(nodes[empty.index()], ProgramNode::Empty) {
        return None;
    }
    let polarity = match (*operator, negated) {
        (LogicalOp::And, false) => AnyAllPolarity::All,
        (LogicalOp::Or, true) => AnyAllPolarity::Any,
        _ => return None,
    };
    let (test, key) = admitted_predicate(nodes, *cond)?;
    Some(AnyAllDemand {
        path,
        filter: CountFilter {
            path: alloc::vec![CountStep::ObjectKey(key)],
            test,
        },
        polarity,
    })
}

fn peel_not(nodes: &[ProgramNode], id: ProgramNodeId) -> (bool, ProgramNodeId) {
    let ProgramNode::FlatMap { upstream, body } = &nodes[id.index()] else {
        return (false, id);
    };
    if is_not_call(nodes, *body) {
        return (true, *upstream);
    }
    (false, id)
}

fn is_not_call(nodes: &[ProgramNode], id: ProgramNodeId) -> bool {
    matches!(
        &nodes[id.index()],
        ProgramNode::Call {
            payload: Evaluator::Not,
            args,
            ..
        } if args.is_empty()
    )
}

fn iterating_current_path(nodes: &[ProgramNode], id: ProgramNodeId) -> Option<Vec<CountStep>> {
    let ProgramNode::Stage {
        start: StageStart::Current,
        steps,
    } = &nodes[id.index()]
    else {
        return None;
    };
    let (last, prefix) = steps.split_last()?;
    if last.is_optional() || !matches!(last.access(), StepAccess::Each) {
        return None;
    }
    static_path(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec_requirement::CodecRequirementPolicy;
    use crate::compile::{CompileOptions, Shortcut, try_compile_program};
    use alloc::vec;
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

    fn compiled(source: &str) -> crate::compile::CompiledProgram {
        let resources = resources();
        let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
        try_compile_program(source, policy, CompileOptions::new(), &resources).expect("compiles")
    }

    fn row(source: &str) -> Option<AnyAllDemand> {
        match compiled(source).shortcut() {
            Shortcut::AnyAll(demand) => Some(demand.clone()),
            _ => None,
        }
    }

    #[test]
    fn any_all_rows_recognize_admitted_predicates() {
        let all = row("all(.ok)").unwrap_or_else(|| panic!("all(.ok): {:?}", compiled("all(.ok)").arena()));
        assert_eq!(all.polarity, AnyAllPolarity::All);
        assert!(all.path.is_empty());
        assert_eq!(all.filter.path, vec![CountStep::ObjectKey("ok".into())]);

        let any = row("any(.ok)").unwrap_or_else(|| panic!("any(.ok): {:?}", compiled("any(.ok)").arena()));
        assert_eq!(any.polarity, AnyAllPolarity::Any);
        assert!(any.path.is_empty());

        let path = row(".users | all(.id)").expect("PATH | all");
        assert_eq!(path.path, vec![CountStep::ObjectKey("users".into())]);
        assert_eq!(path.polarity, AnyAllPolarity::All);

        let generated = row("all(.users[]; .id)").expect("all(PATH[]; P)");
        assert_eq!(generated.path, vec![CountStep::ObjectKey("users".into())]);

        let cmp = row("all(.n > 0)").expect("comparison");
        assert_eq!(cmp.polarity, AnyAllPolarity::All);
        assert!(matches!(cmp.filter.test, jqf_data::CountTest::Compare { .. }));
    }

    #[test]
    fn any_all_rows_decline_identity_optional_and_anti_join() {
        assert!(row("any").is_none(), "identity any");
        assert!(row("all").is_none(), "identity all");
        assert!(row("all(. > 0)").is_none(), "element comparison is not a key predicate");
        assert!(row(".users? | all(.id)").is_none());
        assert!(row("all(.users?[]; .id)").is_none());
        assert!(row(".users[1:2] | all(.id)").is_none());
        // Pipe prefix plus generator path: Exact would locate only `.a`.
        assert!(row(".a | all(.b[]; .k)").is_none());
        // Bound-container generator is the anti-join shape, not this row.
        assert!(row(". as $o | all($o[]; .id)").is_none());
    }
}
