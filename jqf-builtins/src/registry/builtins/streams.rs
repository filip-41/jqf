//! The streaming-utility family: `tostream/0`, `fromstream/1`, and `truncate_stream/1`.
//!
//! One job: own the family/overload records AND the per-law evaluators the executor dispatches through.
//!
//! The laws are the reference's own definitions. `tostream`'s definition uses a RECURSIVE `def`
//! (`path(def r: (.[]?|r), .; r)`), which the engine cannot inline, so it is a native post-order walk; `fromstream` and
//! `truncate_stream` are non-recursive, but they are registered as native laws so the `builtins` enumeration and the
//! demand-transfer receipt stay total over the surface.
//!
//! `tostream` emits the reference's COMPACT stream: a leaf (or empty container) emits `[path, value]`; a non-empty
//! container emits ONLY its LAST child's path as a one-element marker `[path]`; the ROOT pair is never emitted.
//! `{"a":1,"b":[2,3],"c":{"d":4}}` → `[["a"],1] [["b",0],2] [["b",1],3] [["b",1]] [["c","d"],4] [["c","d"]] [["c"]]`.
//!
//! `fromstream` rebuilds the input from that stream, following the reference's own `foreach` definition to the letter:
//! the state is `{x, e}`; each item runs `if $i|length == 2 then setpath(["e"]; $i[0]|length==0) | setpath(["x"]+$i[0];
//! $i[1]) else setpath(["e"]; $i[0]|length==1) end`, a completed root (`e`) is emitted, and the next item resets the
//! state first.
//! Every refusal is the definition's own: a boolean event raises `boolean (…) has no length`, a non-array event
//! raises at `$i[0]`
//! (`Cannot index string with number (0)`), and a non-array PATH raises the `+` mismatch (`array (["x"]) and string
//! ("a") cannot be added`).
//! Nothing is ever silently dropped, and a root completed before a later event raises stays published.
//!
//! `truncate_stream` reads its depth from the PIPED input, runs its argument stream with dot = `null` (the reference's
//! `. as $n | null | stream`), keeps exactly the items whose `(.[0]|length) > $n` under the ORDER comparison, and
//! shortens each kept item to `setpath([0]; $input[0][$n:])`: the path sliced from the AUTHORED depth, so `null` opens
//! the slice, a fractional depth floors (`1.7` truncates like `1`), a negative depth counts from the end, a string
//! depth drops every item (strings rank above numbers in the order), and a boolean depth raises the slice-bound class
//! when an item survives the comparison.

use alloc::vec::Vec;

use jqf_data::{Array, Integer, Number, Object, Value, ValueKind};
use jqf_resource::ResourceContext;

use super::id;
use crate::error::{ArithFailure, ArithMismatchOp, EngineRunError, message};
use crate::registry::builtins::core::owned_length;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};
use crate::semantics::depth;
use crate::semantics::order;
use crate::semantics::owned_kind;
use crate::semantics::path::set_path;
use crate::semantics::{index_owned, resolve_slice_bound};

/// The streaming-utility law discriminants.
#[derive(Clone, Copy, Debug)]
pub enum StreamsLaw {
    /// `tostream/0`.
    Tostream,
    /// `fromstream/1`.
    Fromstream,
    /// `truncate_stream/1`.
    TruncateStream,
}

/// One node of the iterative post-order walk `tostream` runs.
///
/// The frame's position in the SHARED path buffer is a length, not a copy:
/// the walk pushes one path component on descend and truncates on ascend, so a node at depth `d` never allocates a path
/// of its own (the frame stack held one full path copy per level before — O(depth²) component clones per emitted
/// node, the 6.4 KiB/node the RSS lanes observed).
struct TostreamFrame {
    value: Value,
    /// The path buffer's length when this frame was entered (its own step included; the root's is zero).
    depth: usize,
    width: usize,
    next: usize,
    last_child: Option<Value>,
}

fn path_array(elements: &[Value], _resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let mut array = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
    for element in elements {
        array
            .try_push(element.clone())
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    Ok(Value::Array(array))
}

fn pair(path: &[Value], value: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let mut array = Array::try_with_capacity(path.len() + 1).map_err(|_| EngineRunError::allocation_failure())?;
    array
        .try_push(path_array(path, resources)?)
        .map_err(|_| EngineRunError::allocation_failure())?;
    array
        .try_push(value.clone())
        .map_err(|_| EngineRunError::allocation_failure())?;
    Ok(Value::Array(array))
}

fn index_value(index: usize) -> Value {
    Value::Number(Number::integer(Integer::from_i64(
        i64::try_from(index).unwrap_or(i64::MAX),
    )))
}

/// The LAST child's step into `value`, when it is a non-empty container.
///
/// The close marker names where a container ENDED: the reference's `tostream` emits `[node_path + last_child_step]`
/// when a non-empty container closes (`{"a":[1,2]}` closes as `[["a",1]]`), so the last step is the one the marker
/// needs — never the first.
fn last_child_step(value: &Value, _resources: &ResourceContext<'_>) -> Result<Option<Value>, EngineRunError> {
    match value.untagged() {
        Value::Array(array) => {
            let width = array.len();
            if width == 0 {
                Ok(None)
            } else {
                Ok(Some(index_value(width - 1)))
            }
        }
        Value::Object(object) => {
            let mut last = None;
            for entry in object {
                last = Some(
                    entry
                        .try_to_value_string()
                        .map_err(|_| EngineRunError::allocation_failure())?,
                );
            }
            Ok(last)
        }
        _ => Ok(None),
    }
}

/// The width of a container (children count); scalars and empty containers are leaves.
fn width_of(value: &Value) -> usize {
    match value.untagged() {
        Value::Array(array) => array.len(),
        Value::Object(object) => object.len(),
        _ => 0,
    }
}

/// The `next`-th child of a container, with its path step.
fn child_at(value: &Value, next: usize, _resources: &ResourceContext<'_>) -> Result<(Value, Value), EngineRunError> {
    match value.untagged() {
        Value::Array(array) => {
            let child = array
                .get(next)
                .ok_or_else(|| EngineRunError::internal_contract("tostream array child"))?
                .clone();
            Ok((child, index_value(next)))
        }
        Value::Object(object) => {
            let entry = object
                .iter()
                .nth(next)
                .ok_or_else(|| EngineRunError::internal_contract("tostream object child"))?;
            let step = entry
                .try_to_value_string()
                .map_err(|_| EngineRunError::allocation_failure())?;
            let child = entry.value().clone();
            Ok((child, step))
        }
        _ => Err(EngineRunError::internal_contract("tostream child over a leaf")),
    }
}

/// The resumable `tostream/0` walk over one owned input.
///
/// The walk state is exactly what the eager form held — a frame stack plus ONE shared path buffer (components pushed
/// on descend, truncated on ascend, so a node at depth `d` never allocates a path of its own) — but it hands out ONE
/// stream item per [`TostreamWalk::next`] call instead of collecting the whole output into a vector first. The executor
/// drives it from a producer frame, so a wide document's pair sequence streams out at O(one pair) residency instead of
/// O(the whole stream): the materializing collector peaked in the gigabytes on a wide+deep catalog because every pair
/// clones its full path, which only the retained source and the input value need to survive now.
///
/// Emission order and item shapes are the eager walk's, byte for byte: the loop body below is that walk's step,
/// unchanged.
pub struct TostreamWalk {
    /// The suspended post-order walk; empty once every item was emitted.
    stack: Vec<TostreamFrame>,
    /// The CURRENT node's path, shared by every frame at its level.
    path: Vec<Value>,
}

impl TostreamWalk {
    /// Opens the walk at `root`. The root itself never emits (the compact stream law); its frame only seeds the
    /// descent.
    pub fn try_new(root: &Value, resources: &ResourceContext<'_>) -> Result<Self, EngineRunError> {
        let stack = Vec::from([TostreamFrame {
            value: root.clone(),
            depth: 0,
            width: width_of(root),
            next: 0,
            last_child: last_child_step(root, resources)?,
        }]);
        Ok(Self {
            stack,
            path: Vec::new(),
        })
    }

    /// Emits the NEXT stream item — a leaf's `[path, value]` pair or a non-empty container's one-element
    /// `[last-child-path]` marker — or `None` when the walk is spent. One call advances the walk one emission;
    /// nothing past it is computed.
    pub fn next(&mut self, resources: &ResourceContext<'_>) -> Result<Option<Value>, EngineRunError> {
        while let Some(frame) = self.stack.last_mut() {
            if frame.next < frame.width {
                let (child, step) = child_at(&frame.value, frame.next, resources)?;
                frame.next += 1;
                self.path
                    .try_reserve(1)
                    .map_err(|_| EngineRunError::allocation_failure())?;
                self.path.push(step);
                self.stack.push(TostreamFrame {
                    width: width_of(&child),
                    next: 0,
                    last_child: last_child_step(&child, resources)?,
                    depth: self.path.len(),
                    value: child,
                });
                continue;
            }
            let frame = self.stack.pop().expect("the stack is non-empty at the top");
            let emitted = match frame.last_child {
                // A leaf (or empty container): emit `[path, value]`.
                None => pair(&self.path[..frame.depth], &frame.value, resources)?,
                // A non-empty container: emit only `[path + [last_child]]`.
                Some(last_child) => {
                    let mut marker_path =
                        Array::try_with_capacity(frame.depth + 1).map_err(|_| EngineRunError::allocation_failure())?;
                    for component in &self.path[..frame.depth] {
                        marker_path
                            .try_push(component.clone())
                            .map_err(|_| EngineRunError::allocation_failure())?;
                    }
                    marker_path
                        .try_push(last_child)
                        .map_err(|_| EngineRunError::allocation_failure())?;
                    let mut marker = Array::try_with_capacity(1).map_err(|_| EngineRunError::allocation_failure())?;
                    marker
                        .try_push(Value::Array(marker_path))
                        .map_err(|_| EngineRunError::allocation_failure())?;
                    Value::Array(marker)
                }
            };
            // Ascend: drop this frame's own path component.
            self.path.truncate(frame.depth.saturating_sub(1));
            return Ok(Some(emitted));
        }
        Ok(None)
    }
}

/// The `fromstream` state: `{x, e}` — `x` is the document being built and `e` is "emit and reset before the next
/// item".
///
/// The state is an OWNED accumulator, which is what lets the graph machine drive one event at a time: a completed root
/// is published as it lands, and an event that raises keeps every earlier root already published (the `foreach` emits
/// each extract before the next item is evaluated).
pub struct FromstreamState {
    x: Value,
    e: bool,
}

impl FromstreamState {
    pub fn new() -> Self {
        Self {
            x: Value::Null,
            e: false,
        }
    }

    /// One event's update (the reference's `foreach` body) and extract, in the definition's own order: `$i|length`
    /// first (a boolean event raises here), then `$i[0]` through the ordinary index law (a non-array event raises
    /// `Cannot index <kind> with number (0)`, an out-of-range array position reads `null`, and `null` reads `null`),
    /// then the branch's completion test, then — for a two-element event — the write through the `["x"]+$i[0]`
    /// concatenation law.
    ///
    /// Returns the completed root, or `None` when the event did not complete one.
    pub fn step(&mut self, item: &Value, resources: &ResourceContext<'_>) -> Result<Option<Value>, EngineRunError> {
        // `if .e then $init else . end`: a completed root resets the state before the NEXT item is processed.
        if self.e {
            *self = Self::new();
        }
        let item_len = owned_length(item, resources)?;
        let path = index_owned(item, &index_value(0), resources)?;
        let path_len = owned_length(&path, resources)?;
        if length_equals(&item_len, 2) {
            // `setpath(["e"]; $i[0]|length==0) | setpath(["x"]+$i[0]; $i[1])`
            self.e = length_equals(&path_len, 0);
            let Value::Array(pair) = item.untagged() else {
                return Err(EngineRunError::internal_contract(
                    "fromstream two-element event without an array",
                ));
            };
            let value = pair
                .get(1)
                .ok_or_else(|| EngineRunError::internal_contract("fromstream event missing its value"))?;
            self.x = concat_set_path(core::mem::replace(&mut self.x, Value::Null), &path, value, resources)?;
        } else {
            // `setpath(["e"]; $i[0]|length==1)`
            self.e = length_equals(&path_len, 1);
        }
        if self.e {
            let root = self.x.clone();
            Ok(Some(root))
        } else {
            Ok(None)
        }
    }
}

/// Whether the `length` answer equals `target`.
///
/// Exact for every value `length` can produce: a non-negative count (an integer), or the number itself — which the
/// reference's `length` returns unchanged and which can never equal `0`, `1`, or `2` unless it already is one of them.
fn length_equals(value: &Value, target: i64) -> bool {
    match value.untagged() {
        Value::Number(number) => number.to_i64().is_some_and(|length| length == target),
        _ => false,
    }
}

/// The `setpath(["x"]+$i[0]; $i[1])` law in accumulator terms: the state's `"x"` slot is the accumulator, so the
/// remaining path is `["x"]+$i[0]` with the `"x"` component removed — `$i[0]` itself for an array (the element-wise
/// concat), the EMPTY path for `null` (`A + null` is `A`, so a null path writes the root), and the arithmetic mismatch
/// for any other path kind (`array (["x"]) and string ("a") cannot be added` — the reference's `+` refuses to append
/// a non-array to an array).
fn concat_set_path(
    x: Value,
    path: &Value,
    value: &Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let remaining = match path.untagged() {
        Value::Null => Value::Array(Array::try_new().map_err(|_| EngineRunError::allocation_failure())?),
        Value::Array(_) => path.clone(),
        other => {
            let mut left_array = Array::try_with_capacity(1).map_err(|_| EngineRunError::allocation_failure())?;
            left_array
                .try_push(Value::try_string("x").map_err(|_| EngineRunError::allocation_failure())?)
                .map_err(|_| EngineRunError::allocation_failure())?;
            let left = message::dump_trunc_owned(&Value::Array(left_array))?;
            let right = message::dump_trunc_owned(other)?;
            return Err(EngineRunError::Arithmetic {
                failure: ArithFailure::TypeMismatch {
                    op: ArithMismatchOp::Add,
                    left: ValueKind::Array,
                    right: owned_kind(other),
                },
                left,
                right,
            });
        }
    };
    set_path(x, &remaining, value.clone(), resources)
}

/// `truncate_stream/1`'s per-item law, the reference's definition:
/// `(.[0]|length) > $n` under the ORDER comparison, then `setpath([0]; $input[0][$n:])`.
///
/// Returns `Ok(Some(shortened))` for a kept item and `Ok(None)` for a dropped one (the reference's `else empty end`).
///
/// The depth is the AUTHORED piped value, never a normalized number: the comparison is the total order (so a string
/// depth drops every item, a boolean depth keeps them), and the slice's START bound is the authored value (so `null`
/// opens the slice, a number is len-relative with the rounding-to-the-surviving-path law, and a boolean or string bound
/// raises the dedicated slice-bound class).
pub fn truncate_item(
    depth: &Value,
    item: &Value,
    resources: &ResourceContext<'_>,
) -> Result<Option<Value>, EngineRunError> {
    // `(.[0]|length)`: the read is the ordinary index law — a non-array item raises `Cannot index <kind> with number
    // (0)`, an out-of-range array position reads `null`, and `null` reads `null` — and the length is the reference's
    // own (a boolean path raises `boolean (…) has no length`).
    let path = index_owned(item, &index_value(0), resources)?;
    let path_len = owned_length(&path, resources)?;
    let keep = order::observable_cmp(&path_len, depth).map_err(|_| depth::equality_error(resources))?
        == core::cmp::Ordering::Greater;
    if !keep {
        return Ok(None);
    }
    // `setpath([0]; $input[0][$n:])`: the path sliced from the authored depth, then element 0 replaced (a `null` item
    // grows to `[null]` — the set law's null extension).
    let sliced = slice_path(&path, depth, resources)?;
    let mut index_path = Array::try_with_capacity(1).map_err(|_| EngineRunError::allocation_failure())?;
    index_path
        .try_push(index_value(0))
        .map_err(|_| EngineRunError::allocation_failure())?;
    let updated = set_path(item.clone(), &Value::Array(index_path), sliced, resources)?;
    Ok(Some(updated))
}

/// `value[$start:]` over an OWNED value, the slice law the truncate depth drives: `null` yields `null` whatever the
/// bound, an array or string resolves the START bound (a number is len-relative with the authored-sign rounding law, a
/// non-number non-null bound raises the dedicated slice-bound class), and any other input is the index-class mismatch
/// whose operand is the synthesized `{"start": <authored>, "end": null}` object.
fn slice_path(value: &Value, start: &Value, _resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    match value.untagged() {
        Value::Null => Ok(Value::Null),
        Value::Array(array) => {
            let from = slice_start(start, array.len())?;
            let mut out =
                Array::try_with_capacity(array.len() - from).map_err(|_| EngineRunError::allocation_failure())?;
            for element in array.iter().skip(from) {
                out.try_push(element.clone())
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
            Ok(Value::Array(out))
        }
        Value::String(text) => {
            let from = slice_start(start, text.chars().count())?;
            let mut out = alloc::string::String::new();
            for character in text.chars().skip(from) {
                out.try_reserve(character.len_utf8())
                    .map_err(|_| EngineRunError::allocation_failure())?;
                out.push(character);
            }
            Value::try_string(&out).map_err(|_| EngineRunError::allocation_failure())
        }
        other => {
            let mut operand = Object::try_new().map_err(|_| EngineRunError::allocation_failure())?;
            let start_key =
                jqf_data::ObjectKey::try_from_str("start").map_err(|_| EngineRunError::allocation_failure())?;
            operand
                .try_insert_unique(start_key, start.clone())
                .map_err(|_| EngineRunError::allocation_failure())?;
            let end_key = jqf_data::ObjectKey::try_from_str("end").map_err(|_| EngineRunError::allocation_failure())?;
            operand
                .try_insert_unique(end_key, Value::Null)
                .map_err(|_| EngineRunError::allocation_failure())?;
            let rendered = message::render_owned_key(&Value::Object(operand))?;
            Err(EngineRunError::TypeMismatch {
                step_index: 0,
                actual_type: owned_kind(other),
                key: rendered,
                hint: None,
            })
        }
    }
}

/// The resolved START position of `value[$start:]` — `0` for a `null` bound (the open bound), the len-relative
/// authored-sign resolution for a number (NaN behaves as open), or the slice-bound rejection for any other bound.
fn slice_start(start: &Value, len: usize) -> Result<usize, EngineRunError> {
    match start.untagged() {
        Value::Null => Ok(0),
        Value::Number(number) => Ok(resolve_slice_bound(order::to_f64(number), len, true)),
        _ => Err(EngineRunError::SliceIndices),
    }
}

// ------------------------------------------------------------------------ Registry records.

macro_rules! family {
    ($id:expr, $canonical_name:literal, $summary:literal, $detail:literal) => {
        BuiltinFamilyRecord {
            id: BuiltinFamilyId::new($id),
            canonical_name: $canonical_name,
            category: "streams",
            summary: $summary,
            detail: $detail,
        }
    };
}

const TOSTREAM_FAMILY: BuiltinFamilyRecord = family!(
    id::TOSTREAM_FAMILY_ID,
    "tostream",
    "The path/value stream of a document.",
    "Emits the reference's compact `[path, value]` / `[path]` stream: leaves (and \
     empty containers) emit their pair, and every non-empty container emits \
     only its last child's path."
);
const FROMSTREAM_FAMILY: BuiltinFamilyRecord = family!(
    id::FROMSTREAM_FAMILY_ID,
    "fromstream",
    "The stream-to-document rebuild.",
    "Rebuilds one or more documents from a `tostream`-shaped argument stream, \
     emitting each completed root."
);
const TRUNCATE_STREAM_FAMILY: BuiltinFamilyRecord = family!(
    id::TRUNCATE_STREAM_FAMILY_ID,
    "truncate_stream",
    "The stream depth truncator.",
    "Reads a depth from the input, runs the argument stream, drops every item \
     shallower than the depth, and shortens the rest to their first depth \
     segments."
);

pub const FAMILIES: &[BuiltinFamilyRecord] = &[TOSTREAM_FAMILY, FROMSTREAM_FAMILY, TRUNCATE_STREAM_FAMILY];

const ONE_FILTER: &[ParameterKind] = &[ParameterKind::Filter];

const TOSTREAM_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::TOSTREAM),
    family: BuiltinFamilyId::new(id::TOSTREAM_FAMILY_ID),
    canonical_name: "tostream",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[BuiltinExample {
        program: "[tostream]",
        input: "[1,2]",
        expected: "[[[0],1],[[1],2],[[1]]]\n",
    }],
};

const FROMSTREAM_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::FROMSTREAM),
    family: BuiltinFamilyId::new(id::FROMSTREAM_FAMILY_ID),
    canonical_name: "fromstream",
    arity: 1,
    parameters: ONE_FILTER,
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[BuiltinExample {
        program: "[fromstream(tostream)]",
        input: "{\"a\":1,\"b\":[2,3]}",
        expected: "[{\"a\":1,\"b\":[2,3]}]\n",
    }],
};

const TRUNCATE_STREAM_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::TRUNCATE_STREAM),
    family: BuiltinFamilyId::new(id::TRUNCATE_STREAM_FAMILY_ID),
    canonical_name: "truncate_stream",
    arity: 1,
    parameters: ONE_FILTER,
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[BuiltinExample {
        program: "[1 | truncate_stream(([[0,0,1],1], [[0,1],2], [[0,2],3], [[0]]))]",
        input: "null",
        expected: "[[[0,1],1],[[1],2],[[2],3]]\n",
    }],
};

pub const OVERLOADS: &[BuiltinOverloadRecord] = &[TOSTREAM_OVERLOAD, FROMSTREAM_OVERLOAD, TRUNCATE_STREAM_OVERLOAD];

/// The streaming-utility execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Every entry carries its overload id so the const coverage walk in `registry::dispatch` can prove pairwise alignment.
pub const PAYLOADS: &[(u16, StreamsLaw)] = &[
    (id::TOSTREAM, StreamsLaw::Tostream),
    (id::FROMSTREAM, StreamsLaw::Fromstream),
    (id::TRUNCATE_STREAM, StreamsLaw::TruncateStream),
];

#[cfg(test)]
mod tostream_marker_tests {
    use super::*;
    use crate::semantics::render::write_value;
    use alloc::{string::String, vec};
    use jqf_data::{ObjectBuilder, ObjectKey};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
    }

    fn integer(value: i64) -> Value {
        Value::Number(Number::integer(Integer::from_i64(value)))
    }

    fn array(items: Vec<Value>) -> Value {
        Value::Array(Array::try_from_vec(items).expect("array"))
    }

    fn object(entries: Vec<(&str, Value)>) -> Value {
        let mut builder = ObjectBuilder::try_with_capacity(entries.len()).expect("builder");
        for (key, value) in entries {
            builder
                .try_insert_last(ObjectKey::try_from_str(key).expect("key"), value)
                .expect("insert");
        }
        Value::Object(builder.try_finish().expect("object"))
    }

    /// The close-marker law, byte-verified against the reference: a non-empty container closes with `[[path + LAST
    /// child step]]` — an array's last INDEX, an object's last KEY — and a leaf (or empty container) closes with
    /// `[path, value]`. The last-child step is the whole point of the marker; a first-child step would emit `[["a",0]]`
    /// where the reference emits `[["a",1]]`.
    #[test]
    fn container_close_markers_carry_the_last_child_step() {
        let resources = resources();
        let input = object(vec![("a", array(vec![integer(1), integer(2)])), ("b", integer(3))]);
        let mut walk = TostreamWalk::try_new(&input, &resources).expect("tostream walk");
        let mut outputs = Vec::new();
        while let Some(value) = walk.next(&resources).expect("tostream item") {
            outputs.push(value);
        }
        let rendered: Vec<String> = outputs
            .iter()
            .map(|value| {
                let mut line = String::new();
                write_value(&mut line, value).expect("render");
                line
            })
            .collect();
        assert_eq!(
            rendered.join("\n"),
            "[[\"a\",0],1]\n[[\"a\",1],2]\n[[\"a\",1]]\n[[\"b\"],3]\n[[\"b\"]]"
        );
    }
}
