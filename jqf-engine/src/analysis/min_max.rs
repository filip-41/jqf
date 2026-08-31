//! The `min`/`max`/`min_by`/`max_by` compile shortcut: a static container
//! path plus a numeric array (or numeric probe), answered without the graph.
//!
//! Rows: `PATH | min` / `PATH | max` (`Call Whole(Min|Max)`) and
//! `PATH | min_by(.k)` / `PATH | max_by(.k)` (`Call Keyed(Min|Max, [single-Key
//! stage])`). Empty PATH is the document root (`min`, `max`, `min_by(.k)`).
//!
//! The oracle answers only an **array of finite numbers**, or `min_by`/`max_by`
//! whose per-element probe is a finite number. Empty array is `null`. Mixed
//! kinds, NaN, Inf, a non-array, and a missing PATH all Decline: the graph owns
//! the catalogued NaN total order and the iterate-null raise. Do not reuse
//! `has`'s located-null arm.
//!
//! `[gen] | min_by` is already stream-fused (`CollectArray`); it is not a row.
//! `min_by(.)`, a slice/optional prefix, and a multi-output key decline.

use alloc::string::String;
use alloc::vec::Vec;

use super::path_steps::static_path;
use crate::program::{ProgramNode, ProgramNodeId, StageStart, StepAccess};
use jqf_builtins::registry::Evaluator;
use jqf_builtins::registry::builtins::order::WholeForm;
use jqf_builtins::semantics::keyed::KeyMode;
use jqf_data::{CountStep, MinMaxOp};

/// A recognized `min`/`max`/`min_by`/`max_by` shortcut.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MinMaxDemand {
    /// Static Key/Index steps to the array. Empty is the document root.
    pub path: Vec<CountStep>,
    /// Smallest vs largest.
    pub op: MinMaxOp,
    /// `None` is `min`/`max` (the element is the key). `Some` is the single
    /// object key `min_by`/`max_by` probes.
    pub probe: Option<String>,
}

/// The recognized min/max shortcut, or `None` when the program is not a row.
pub(crate) fn min_max_demand(nodes: &[ProgramNode], root: ProgramNodeId) -> Option<MinMaxDemand> {
    if let Some(row) = min_max_call(nodes, root) {
        return Some(row);
    }
    let ProgramNode::FlatMap { upstream, body } = &nodes[root.index()] else {
        return None;
    };
    // CollectArray | min[_by] is the stream-fused spelling, not this row.
    if matches!(&nodes[upstream.index()], ProgramNode::CollectArray { .. }) {
        return None;
    }
    let mut row = min_max_call(nodes, *body)?;
    row.path = match &nodes[upstream.index()] {
        ProgramNode::Stage {
            start: StageStart::Current,
            steps,
        } => static_path(steps)?,
        _ => return None,
    };
    Some(row)
}

fn min_max_call(nodes: &[ProgramNode], id: ProgramNodeId) -> Option<MinMaxDemand> {
    let ProgramNode::Call { payload, args, .. } = &nodes[id.index()] else {
        return None;
    };
    let (op, probe) = match payload {
        Evaluator::Whole(WholeForm::Min) if args.is_empty() => (MinMaxOp::Min, None),
        Evaluator::Whole(WholeForm::Max) if args.is_empty() => (MinMaxOp::Max, None),
        Evaluator::Keyed(KeyMode::Min) => (MinMaxOp::Min, Some(single_key_arg(nodes, args)?)),
        Evaluator::Keyed(KeyMode::Max) => (MinMaxOp::Max, Some(single_key_arg(nodes, args)?)),
        _ => return None,
    };
    Some(MinMaxDemand {
        path: Vec::new(),
        op,
        probe,
    })
}

/// One un-optional Key step, the only probe `min_by`/`max_by` admits.
fn single_key_arg(nodes: &[ProgramNode], args: &[ProgramNodeId]) -> Option<String> {
    let [arg] = args else {
        return None;
    };
    let ProgramNode::Stage {
        start: StageStart::Current,
        steps,
    } = &nodes[arg.index()]
    else {
        return None;
    };
    if steps.len() != 1 || steps[0].is_optional() {
        return None;
    }
    match steps[0].access() {
        StepAccess::Key(key) => Some(key.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec_requirement::CodecRequirementPolicy;
    use crate::compile::{CompileOptions, Shortcut, try_compile_program};
    use alloc::vec;
    use jqf_codec_core::{DiagnosticPolicy, ValidationMode};
    use jqf_data::MinMaxOp;
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

    fn row(source: &str) -> Option<MinMaxDemand> {
        match compiled(source).shortcut() {
            Shortcut::MinMax(demand) => Some(demand.clone()),
            _ => None,
        }
    }

    #[test]
    fn min_max_rows_recognize_path_and_probe() {
        let min = row("min").expect("bare min");
        assert_eq!(min.op, MinMaxOp::Min);
        assert!(min.path.is_empty());
        assert!(min.probe.is_none());

        let max = row("max").expect("bare max");
        assert_eq!(max.op, MinMaxOp::Max);
        assert!(max.path.is_empty());

        let path = row(".xs | min").expect("PATH | min");
        assert_eq!(path.path, vec![CountStep::ObjectKey("xs".into())]);
        assert_eq!(path.op, MinMaxOp::Min);
        assert!(path.probe.is_none());

        let path_max = row(".xs | max").expect("PATH | max");
        assert_eq!(path_max.path, vec![CountStep::ObjectKey("xs".into())]);
        assert_eq!(path_max.op, MinMaxOp::Max);

        let by = row("min_by(.n)").expect("min_by");
        assert!(by.path.is_empty());
        assert_eq!(by.op, MinMaxOp::Min);
        assert_eq!(by.probe.as_deref(), Some("n"));

        let path_by = row(".xs | min_by(.n)").expect("PATH | min_by");
        assert_eq!(path_by.path, vec![CountStep::ObjectKey("xs".into())]);
        assert_eq!(path_by.probe.as_deref(), Some("n"));

        let max_by = row(".xs | max_by(.n)").expect("PATH | max_by");
        assert_eq!(max_by.op, MinMaxOp::Max);
        assert_eq!(max_by.probe.as_deref(), Some("n"));
    }

    #[test]
    fn min_max_rows_decline_collect_identity_optional_and_slice() {
        assert!(
            row("[.[] | .n] | min_by(.n)").is_none(),
            "[gen] | min_by is stream-fused"
        );
        assert!(row("min_by(.)").is_none(), "identity key");
        assert!(row("max_by(.)").is_none(), "identity key");
        assert!(row("min_by(.a, .b)").is_none(), "multi-output key");
        assert!(row(".xs? | min").is_none());
        assert!(row(".xs[1:2] | min").is_none());
        assert!(row(".xs | min?").is_none());
        assert!(row("min_by(.[0])").is_none(), "index probe");
        assert!(row(".xs[] | min").is_none(), "each on the path");
    }
}
