//! Shared imports for the compile sibling modules.

pub(crate) use alloc::borrow::ToOwned;
pub(crate) use alloc::boxed::Box;
pub(crate) use alloc::collections::BTreeSet;
pub(crate) use alloc::string::String;
pub(crate) use alloc::vec;
pub(crate) use alloc::vec::Vec;
pub(crate) use core::fmt;

#[cfg(test)]
extern crate std;

pub(crate) use jqf_codec_core::{AccessRequirement, CodecError, CodecFailureKind, DemandClause};
pub(crate) use jqf_resource::policy::MismatchPolicy;
pub(crate) use jqf_resource::{ResourceContext, ResourceError};
pub(crate) use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef, Span};
pub(crate) use jqf_syntax::{
    AccessorSelector, AssignmentExpr, AssignmentOp, BinaryOp, CallArgument, CallExpr, ConditionalExpr, DefParameter,
    Expr, ExprKind, FieldSelector, ImportItem, IncludeItem, LoopExpr, ObjectKey, ObjectMember, Pattern, PatternKind,
    PostfixExpr, PostfixSegment, PostfixStep, SourceItem, StringTemplate, SyntaxInputError, SyntaxSource,
    TemplateSegment, UnaryOp, parse_program, parse_query,
};

pub(crate) use jqf_data::{Array, ExpandedName, Integer, Number, Value};

pub(crate) use crate::analysis::{BoundaryConsumer, ProjectionClass, analyze};
pub(crate) use crate::codec_requirement::{
    CodecRequirementPolicy, StaticForwardStep, try_lower_forward_requirement, try_lower_prune_tree,
    try_lower_root_requirement,
};
pub(crate) use crate::exec::{EngineRun, try_run_program};
pub(crate) use crate::program::{
    BinaryKind, CallableDef, CountedKind, EnginePullKind, EngineSlot, LabelSlot, LogicalOp, ModifyMode,
    ObjectMemberNode, Program, ProgramNode, ProgramNodeId, SliceBound, SliceBounds, StageStart, StageStep, StepAccess,
    VarSlot,
};
pub(crate) use jqf_builtins::codec_result::{CodecInputOutcome, EngineResult};
pub(crate) use jqf_builtins::constant::{
    constant_object_key, decode_literal_segment, evaluate_constant, lower_number, static_template_text,
};
pub(crate) use jqf_builtins::registry::{
    BuiltinDispatch, BuiltinExecution, Evaluator, Lowering, dispatch, resolve_builtin,
};
pub(crate) use jqf_builtins::semantics::arith::{ArithOp, compute_number};
