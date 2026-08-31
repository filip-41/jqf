//! Shared imports for the executor sibling modules.
//!
//! One job: keep every `exec` sibling on the same import set the original
//! `mod.rs` carried, so a mechanical extract does not invent a second
//! dependency surface.

pub(crate) use alloc::borrow::ToOwned;
pub(crate) use alloc::boxed::Box;
pub(crate) use alloc::string::String;
pub(crate) use alloc::vec;
pub(crate) use alloc::vec::Vec;

pub(crate) use jqf_codec_core::{CodecError, CodecFailureKind, LocatedProduct};
pub(crate) use jqf_data::{
    Array, Integer, MaterializeWorkspace, NodeHandle, NodeId, Number, ObjectBuilder, Value, ValueKind,
};
pub(crate) use jqf_resource::{ControlError, OwnedDepthGuard, ResourceContext, ResourceError, WorkAdmission};

pub(crate) use super::stage::{
    Container, Descended, WRITABLE_NODE_ROLES, descend, descend_borrowed, next_child, resolve_fact_write_selector,
    walk_fact_path,
};
pub(crate) use crate::analysis::{AntiJoinScan, CorrelatedScan, PartialSort, TopKConsumer};
pub(crate) use crate::program::{
    BinaryKind, BinaryShape, CountedKind, EnginePullKind, EngineSlot, FilterSlot, LabelSlot, LogicalOp, ModifyMode,
    ObjectMemberNode, Program, ProgramNode, ProgramNodeId, StageStart, StageStep, StepAccess, VarSlot,
};
pub(crate) use jqf_builtins::codec_result::{CodecInputOutcome, EngineResult};
pub(crate) use jqf_builtins::registry::Evaluator;
pub(crate) use jqf_builtins::registry::builtins::core as core_builtins;
pub(crate) use jqf_builtins::registry::builtins::entries as entries_builtins;
#[cfg(feature = "ext-hash")]
pub(crate) use jqf_builtins::registry::builtins::extension as extension_builtins;
pub(crate) use jqf_builtins::registry::builtins::facts as facts_builtins;
pub(crate) use jqf_builtins::registry::builtins::format as format_builtins;
#[cfg(feature = "ext-jsonpath")]
pub(crate) use jqf_builtins::registry::builtins::jsonpath as jsonpath_builtins;
pub(crate) use jqf_builtins::registry::builtins::kinds as kind_builtins;
pub(crate) use jqf_builtins::registry::builtins::math as math_builtins;
#[cfg(feature = "ext-net")]
pub(crate) use jqf_builtins::registry::builtins::net as net_builtins;
pub(crate) use jqf_builtins::registry::builtins::order as order_builtins;
pub(crate) use jqf_builtins::registry::builtins::parse as parser_builtins;
pub(crate) use jqf_builtins::registry::builtins::pointer as pointer_builtins;
pub(crate) use jqf_builtins::registry::builtins::process as process_builtins;
pub(crate) use jqf_builtins::registry::builtins::regex as regex_builtins;
pub(crate) use jqf_builtins::registry::builtins::reshape as reshape_builtins;
pub(crate) use jqf_builtins::registry::builtins::rider as rider_builtins;
#[cfg(feature = "ext-schema")]
pub(crate) use jqf_builtins::registry::builtins::schema as schema_builtins;
pub(crate) use jqf_builtins::registry::builtins::search::{self as search_builtins, TextLaw};
pub(crate) use jqf_builtins::registry::builtins::selector as selector_builtins;
pub(crate) use jqf_builtins::registry::builtins::streams as streams_builtins;
pub(crate) use jqf_builtins::registry::builtins::strings as string_builtins;
pub(crate) use jqf_builtins::registry::builtins::text as text_builtins;
pub(crate) use jqf_builtins::registry::builtins::time::{self as time_builtins, TimeEvaluator, TimeFormatLaw, TimeLaw};
pub(crate) use jqf_builtins::registry::builtins::top_k as top_k_builtins;
pub(crate) use jqf_builtins::semantics::generate::{
    self, Dimensions, Generator, Odometer, RangeBounds, RangeCursor, RangeLaw, Termination, ValueCursor,
};
pub(crate) use jqf_builtins::semantics::path as semantics_path;
pub(crate) use jqf_builtins::semantics::{Prng, rand_float};
pub(crate) use jqf_builtins::semantics::{binary, order, truth};
