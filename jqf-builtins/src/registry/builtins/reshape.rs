//! The reshaping family and the index idioms: `add`, `flatten`, `transpose`, `has`, `in`, `walk`, `map_values`, `pick`,
//! `IN`, `INDEX`, `JOIN`.
//!
//! One job: own the family and overload records for the vocabulary that RESTRUCTURES a container — collapsing it
//! (`add`, `flatten`), pivoting it (`transpose`), rebuilding it in place (`walk`, `map_values`, `pick`), asking about
//! its shape (`has`, `in`, `IN`) or re-keying it (`INDEX`, `JOIN`) — plus the three evaluator payloads that do the
//! collapsing, pivoting and asking.
//!
//! Most of the family is a REGISTERED LOWERING of the reference's own definition, which is where the edge cases come
//! from for free:
//!
//! ```text
//! def add:              reduce .[] as $x (null; . + $x);
//! def add(f):           reduce f    as $x (null; . + $x);
//! def in(xs):           . as $x | xs | has($x);
//! def map_values(f):    .[] |= f;
//! def pick(pathexps):   . as $top | reduce path(pathexps) as $p
//!                           (null; setpath($p; $top | getpath($p)));
//! def IN(s):            any(s == .;   .);
//! def IN(src; s):       any(s == src; .);
//! def INDEX(stream; idx_expr): reduce stream as $row ({}; .[$row|idx_expr|tostring] = $row);
//! def INDEX(idx_expr):         INDEX(.[]; idx_expr);
//! def JOIN($idx; idx_expr):                     [.[] | [., $idx[idx_expr]]];
//! def JOIN($idx; stream; idx_expr):             stream | [., $idx[idx_expr]];
//! def JOIN($idx; stream; idx_expr; join_expr):  stream | [., $idx[idx_expr]] | join_expr;
//! ```
//!
//! **Two of those spellings cannot be transcribed and are REBOUND instead**, and that is this family's one deliberate
//! mechanical adaptation. The reference's `.[idx_expr]` and `$idx[idx_expr]` index a container by an ARBITRARY
//! expression; jqf's `Stage` addresses a dynamic component only through a variable slot. So `INDEX` and `JOIN` bind the
//! key first and index by the binding:
//!
//! ```text
//! INDEX(stream; f)  ->  reduce stream as $row ({};
//!                           reduce ($row|f|tostring) as $k (.; .[$k] = $row))
//! $idx[idx_expr]    ->  (idx_expr) as $k | $idx | .[$k]
//! ```
//!
//! The INNER REDUCE is not decoration. The reference's `.[…] = $row` is one assignment over however many paths the
//! key expression names, so `INDEX(.a,.b)` sets BOTH keys in one object and `INDEX(empty)` leaves the accumulator
//! untouched. A plain `as $k | .[$k] = $row` would instead publish one object PER key and let `reduce` keep the last
//! (losing `.a`), and would publish nothing at all for `empty` (nulling the accumulator). Folding over the key stream
//! from the accumulator reproduces both.
//!
//! `add/0`, `flatten`, `transpose` and `has` are evaluators here — `add/0`'s law IS the `def add:` reduce above,
//! folded natively because the per-element reduce cycle was the cost of `add`-heavy lanes (`add(f)` keeps the lowering:
//! its source is an arbitrary filter, which only the machine can drive). `walk/1` is an evaluator too, but its payload
//! is a FRAME in [`crate::exec`] rather than a pure function: it rebuilds a whole subtree bottom-up while running a
//! filter at every level, which is a subgraph drive.
//!
//! Negative space: it owns no ordering (`INDEX`'s object keeps first-insertion order, which is `ObjectBuilder`'s own
//! law), no path machinery (`pick` reaches it through `path`/`getpath`/`setpath`) and no arithmetic (`add` reaches `+`
//! through the ordinary binary operator).

use alloc::string::String;
use alloc::vec::Vec;

use jqf_data::{Array, Number, Object, ObjectBuilder, ObjectEntry, Value, ValueKind, ValueView};
use jqf_resource::ResourceContext;

use super::id;
use crate::codec_result::EngineResult;
use crate::error::EngineRunError;
use crate::error::message;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};
use crate::semantics::binary::BinaryKind;
use crate::semantics::order;
use crate::semantics::path;
use crate::semantics::path::{get_component, raise};

/// The reshaping family records.
pub const FAMILIES: &[BuiltinFamilyRecord] = &[
    ADD_FAMILY,
    FLATTEN_FAMILY,
    TRANSPOSE_FAMILY,
    HAS_FAMILY,
    IN_FAMILY,
    WALK_FAMILY,
    MAP_VALUES_FAMILY,
    PICK_FAMILY,
    IN_STREAM_FAMILY,
    INDEX_FAMILY,
    JOIN_INDEXED_FAMILY,
];

/// The reshaping overload records.
pub const OVERLOADS: &[BuiltinOverloadRecord] = &[
    ADD_OVERLOAD,
    ADD_ONE_OVERLOAD,
    FLATTEN_OVERLOAD,
    FLATTEN_ONE_OVERLOAD,
    TRANSPOSE_OVERLOAD,
    HAS_OVERLOAD,
    IN_OVERLOAD,
    WALK_OVERLOAD,
    MAP_VALUES_OVERLOAD,
    PICK_OVERLOAD,
    IN_STREAM_OVERLOAD,
    IN_STREAM_TWO_OVERLOAD,
    INDEX_OVERLOAD,
    INDEX_TWO_OVERLOAD,
    JOIN_TWO_OVERLOAD,
    JOIN_THREE_OVERLOAD,
    JOIN_FOUR_OVERLOAD,
];

/// The reshaping execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// The family spans evaluators and lowerings, so it names its own payload enum; `registry::dispatch` wraps it at table
/// build time. Every entry carries its overload id so the const coverage walk there can prove pairwise alignment.
#[derive(Clone, Copy, Debug)]
pub enum ReshapePayload {
    /// `add/0` — the native `+` fold over the input's children.
    Add,
    /// `add/1` — expands to `reduce <source> as $x (null; . + $x)`.
    AddLowered,
    /// `flatten/0` and `flatten/1` — the nested-array splice.
    Flatten,
    /// `transpose/0` — the null-padded row/column pivot.
    Transpose,
    /// `has/1` — the shallow key-presence test.
    Has,
    /// `in/1` — expands to `. as $x | xs | has($x)`.
    In,
    /// `walk/1` — the bottom-up rebuild.
    Walk,
    /// `map_values/1` — the per-member rebuild.
    MapValues,
    /// `pick/1` — expands to the `path`/`getpath`/`setpath` skeleton rebuild.
    Pick,
    /// `IN/1` and `IN/2` — expand to `any(s == <subject>; .)`.
    InStream,
    /// `INDEX/1` and `INDEX/2` — expand to the re-keying fold.
    Index,
    /// `JOIN/2..4` — expand to the index left join.
    JoinIndexed,
}

pub const PAYLOADS: &[(u16, ReshapePayload)] = &[
    (id::ADD_ZERO, ReshapePayload::Add),
    (id::ADD_ONE, ReshapePayload::AddLowered),
    (id::FLATTEN_ZERO, ReshapePayload::Flatten),
    (id::FLATTEN_ONE, ReshapePayload::Flatten),
    (id::TRANSPOSE, ReshapePayload::Transpose),
    (id::HAS, ReshapePayload::Has),
    (id::IN_KEY, ReshapePayload::In),
    (id::WALK, ReshapePayload::Walk),
    (id::MAP_VALUES, ReshapePayload::MapValues),
    (id::PICK, ReshapePayload::Pick),
    (id::IN_STREAM_ONE, ReshapePayload::InStream),
    (id::IN_STREAM_TWO, ReshapePayload::InStream),
    (id::INDEX_ONE, ReshapePayload::Index),
    (id::INDEX_TWO, ReshapePayload::Index),
    (id::JOIN_TWO, ReshapePayload::JoinIndexed),
    (id::JOIN_THREE, ReshapePayload::JoinIndexed),
    (id::JOIN_FOUR, ReshapePayload::JoinIndexed),
];

/// The refusal for a negative `flatten` depth, checked once at the TOP level.
const NEGATIVE_DEPTH: &str = "flatten depth must not be negative";

/// Evaluates `add` over an owned input: the reference's own `reduce .[] as $x (null; . + $x)`, folded natively.
///
/// Everything observable is `.[]`'s and `+`'s, not this function's: the children are an array's elements or an object's
/// VALUES in jqf's own `.[]` order, a non-iterable input raises `.[]`'s iterate error, the `null` seed is why an empty
/// input answers `null`, and every step is the ordinary binary `+` through [`crate::error::apply_binary`] — same
/// results and same messages as the registered lowering this fold replaces. Only the fold's DRIVER changed: a loop here
/// instead of one graph-machine reduce cycle per element, which was the cost of `add`-heavy generator lanes (a
/// `combinations | add` fold spent its time in frame machinery, not in `+`).
///
/// # Concatenating children fold in ONE pass
///
/// `+` over two arrays, two strings or two objects builds a WHOLE new container sized to both operands. Folding that
/// pairwise makes the accumulator the left operand every time, so the fold copies the running result once per child and
/// the whole `add` is quadratic in the output — which is exactly what the `add_nested_arrays` lane measures ("the
/// concatenating add, where a copying `+` shows up as quadratic").
///
/// So the three concatenating kinds accumulate into ONE output container instead: each child appended once, linear in
/// the answer. `+` is associative for all three, and the object merge's first-position/last-value law is exactly what
/// `ObjectBuilder::try_finish` applies to occurrences collected in child order, so the answer is the pairwise fold's
/// answer.
///
/// The fast path is taken only when every child is literally that kind, with `Value::Null` allowed anywhere because it
/// is `+`'s identity. Anything else — numbers, a tagged payload, a mixed sequence, a type error — falls to the
/// pairwise loop, which owns every result and every message as before.
///
/// The one thing the fast path does NOT reproduce is the pairwise fold's LEDGER TRACE: it charges one output container
/// rather than a chain of discarded intermediates. That is the point of the change, and it charges strictly less.
///
/// # Errors
///
/// Returns the iterate error for a non-iterable input, whatever binary `+` raises for a mismatched pair, or an
/// allocation failure.
pub fn add(input: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    match input.untagged() {
        Value::Array(array) => add_children(array.iter(), resources),
        // `.[]` over an object is its VALUES, folded the same way.
        Value::Object(object) => add_children(object.into_iter().map(ObjectEntry::value), resources),
        other => {
            let operand = message::dump_trunc_owned(other)?;
            let text = message::iterate_message(other.kind(), &operand)?;
            Err(raise(&text, resources))
        }
    }
}

/// The children of `.[]` folded under `+`, concatenation in one pass.
fn add_children<'value>(
    children: impl Iterator<Item = &'value Value> + Clone,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    match concatenating_kind(children.clone()) {
        Some(ValueKind::Array) => {
            let width = children.clone().filter_map(as_array).map(Array::len).sum();
            let mut out = Array::try_with_capacity(width).map_err(|_| EngineRunError::allocation_failure())?;
            for element in children.filter_map(as_array).flatten() {
                out.try_push(path::try_clone(element))
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
            Ok(Value::Array(out))
        }
        Some(ValueKind::String) => {
            let mut out = String::new();
            out.try_reserve_exact(children.clone().filter_map(as_str).map(str::len).sum())
                .map_err(|_| EngineRunError::allocation_failure())?;
            children.filter_map(as_str).for_each(|text| out.push_str(text));
            Value::try_string(&out).map_err(|_| EngineRunError::allocation_failure())
        }
        Some(ValueKind::Object) => {
            let width = children.clone().filter_map(as_object).map(Object::len).sum();
            let mut builder =
                ObjectBuilder::try_with_capacity(width).map_err(|_| EngineRunError::allocation_failure())?;
            for entry in children.filter_map(as_object).flatten() {
                builder
                    .try_insert_last(entry.clone_key(), path::try_clone(entry.value()))
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
            // `try_finish`, not `try_finish_unique`: two children may carry the same key, and this is where `+`'s
            // last-value-wins is settled.
            builder
                .try_finish()
                .map(Value::Object)
                .map_err(|_| EngineRunError::allocation_failure())
        }
        // Not a homogeneous concatenation: the ordinary pairwise fold, which owns every mismatch message and every
        // numeric result unchanged.
        _ => {
            let mut sum = Value::Null;
            for child in children {
                sum = crate::error::apply_binary(BinaryKind::Add, &sum, child, resources)?;
            }
            Ok(sum)
        }
    }
}

/// The one concatenating kind every non-null child shares, or `None` when the children are mixed, tagged, empty, or of
/// any other kind.
fn concatenating_kind<'value>(children: impl Iterator<Item = &'value Value>) -> Option<ValueKind> {
    let mut seen = None;
    for child in children {
        let kind = match child {
            // `+`'s identity: it neither picks the kind nor breaks it.
            Value::Null => continue,
            Value::Array(_) => ValueKind::Array,
            Value::String(_) => ValueKind::String,
            Value::Object(_) => ValueKind::Object,
            _ => return None,
        };
        match seen {
            None => seen = Some(kind),
            Some(previous) if previous == kind => {}
            Some(_) => return None,
        }
    }
    seen
}

fn as_array(value: &Value) -> Option<&Array> {
    match value {
        Value::Array(array) => Some(array),
        _ => None,
    }
}

fn as_str(value: &Value) -> Option<&str> {
    match value {
        Value::String(text) => Some(text),
        _ => None,
    }
}

fn as_object(value: &Value) -> Option<&Object> {
    match value {
        Value::Object(object) => Some(object),
        _ => None,
    }
}

/// Evaluates `flatten` / `flatten($depth)` over an owned input.
///
/// `depth` is `None` for the arity-0 form, which the reference spells as the unlimited depth `-1` rather than as a
/// large finite one.
///
/// **Two facts here are not the obvious reading.** The recursion descends while the depth is `!= 0` — NOT while it is
/// positive — so a FRACTIONAL depth never reaches zero and flattens all the way down: `[[[[[1]]]]] | flatten(0.5)` is
/// `[1]` while `flatten(2)` on the same input stops at `[[[1]]]`. And the negative refusal is a TOP-LEVEL check that
/// the recursion does not repeat, which is what lets the countdown pass through negative depths at all.
///
/// The refusal itself compares under the TOTAL ORDER, so `null`, `false`, `true` and `nan` are all "negative" while a
/// string or an array is not. The subtraction is reached only when an ARRAY element is actually descended, which is why
/// `[1,2] | flatten("a")` succeeds while `[1,[2]] | flatten("a")` raises the ordinary subtraction type error.
///
/// The top level iterates any ITERABLE — `{"a":[1]} | flatten` is `[1]` — while the recursion descends ARRAYS
/// alone, so an object nested inside an array is a leaf.
///
/// The walk is an explicit stack and never the Rust one: a `flatten` over a ten-thousand-deep array is a reference test
/// case, not a hypothetical.
///
/// # Errors
///
/// Returns the negative-depth refusal, the iterate error for a non-iterable input, the ordinary subtraction error for a
/// non-numeric depth that is actually spent, or an allocation failure.
pub fn flatten(input: &Value, depth: Option<&Value>, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    if let Some(depth) = depth
        && order::total_cmp(depth, &integer(0)).map_err(|_| depth_too_deep())? == core::cmp::Ordering::Less
    {
        return Err(raise(NEGATIVE_DEPTH, resources));
    }
    let mut out = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
    let mut stack: Vec<Level> = Vec::new();
    stack.try_reserve(1).map_err(|_| EngineRunError::allocation_failure())?;
    stack.push(Level {
        children: top_children(input, resources)?,
        cursor: 0,
        depth: depth.cloned(),
    });
    while let Some(level) = stack.last_mut() {
        let Some(child) = level.children.get(level.cursor) else {
            stack.pop();
            continue;
        };
        level.cursor += 1;
        let descend = matches!(child.untagged(), Value::Array(_)) && level.spends();
        let child = child.clone();
        if !descend {
            out.try_push(child).map_err(|_| EngineRunError::allocation_failure())?;
            continue;
        }
        // The subtraction happens HERE and nowhere else: only a descent spends depth, which is what keeps a non-numeric
        // depth harmless over a flat array and fatal over a nested one.
        let inner = match &level.depth {
            Some(depth) => Some(crate::error::apply_binary(
                BinaryKind::Subtract,
                depth,
                &integer(1),
                resources,
            )?),
            None => None,
        };
        let Value::Array(array) = child.untagged() else {
            return Err(EngineRunError::internal_contract("a flatten descent into a non-array"));
        };
        let mut children = Vec::new();
        children
            .try_reserve_exact(array.len())
            .map_err(|_| EngineRunError::allocation_failure())?;
        for element in array {
            children.push(element.clone());
        }
        stack.try_reserve(1).map_err(|_| EngineRunError::allocation_failure())?;
        stack.push(Level {
            children,
            cursor: 0,
            depth: inner,
        });
    }
    Ok(Value::Array(out))
}

/// One suspended `flatten` level: the children still to visit and the depth budget they are visited under.
struct Level {
    children: Vec<Value>,
    cursor: usize,
    depth: Option<Value>,
}

impl Level {
    /// Whether an array child at this level is DESCENDED rather than appended whole.
    ///
    /// The test is the reference's `$x != 0` and not `$x > 0`, which is the whole reason a fractional depth flattens
    /// without limit: the countdown steps straight past zero and never lands on it. An absent depth is the arity-0
    /// form's unlimited `-1`, which never lands on zero either.
    fn spends(&self) -> bool {
        match &self.depth {
            None => true,
            Some(depth) => !order::semantic_eq(depth, &integer(0)).unwrap_or(false),
        }
    }
}

/// `flatten`'s top-level children: an array's elements or an OBJECT's values, because the reference reaches them
/// through `.[]`.
fn top_children(input: &Value, resources: &ResourceContext<'_>) -> Result<Vec<Value>, EngineRunError> {
    let mut children = Vec::new();
    match input.untagged() {
        Value::Array(array) => {
            children
                .try_reserve_exact(array.len())
                .map_err(|_| EngineRunError::allocation_failure())?;
            for element in array {
                children.push(element.clone());
            }
        }
        Value::Object(object) => {
            children
                .try_reserve_exact(object.len())
                .map_err(|_| EngineRunError::allocation_failure())?;
            for entry in object {
                children.push(entry.value().clone());
            }
        }
        other => {
            let operand = message::dump_trunc_owned(other)?;
            let text = message::iterate_message(other.kind(), &operand)?;
            return Err(raise(&text, resources));
        }
    }
    Ok(children)
}

/// Evaluates `transpose` over an owned input.
///
/// The reference's shape, read as one function:
///
/// ```text
/// [range(0; (map(length) | max) // 0) as $j | [.[] | .[$j]]]
/// ```
///
/// The `// 0` is why `{}` and `[null]` both answer `[]` rather than raising on `range(0; null)`. The two phases are the
/// whole error law: EVERY child's length is taken before ANY child is indexed, so `[[1],true]` is a `has no length`
/// error and never an index error, while `[[1],"ab"]` measures fine and only then fails to index the string.
///
/// # Errors
///
/// Returns the iterate error for a non-iterable input, the no-length error for a child with no length, the index error
/// for a child that cannot be indexed by a number, or an allocation failure.
pub fn transpose(input: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let children = top_children(input, resources)?;
    let mut width = 0usize;
    for child in &children {
        let length = super::core::owned_length(child, resources)?;
        let Value::Number(number) = length.untagged() else {
            return Err(EngineRunError::internal_contract("a length that is not a number"));
        };
        width = width.max(column_count(number));
    }
    let mut columns = Array::try_with_capacity(width).map_err(|_| EngineRunError::allocation_failure())?;
    for position in 0..width {
        let index = i64::try_from(position)
            .map_err(|_| EngineRunError::internal_contract("transpose column index overflow"))?;
        let component = integer(index);
        let mut column = Array::try_with_capacity(children.len()).map_err(|_| EngineRunError::allocation_failure())?;
        for child in &children {
            let cell = get_component(child, &component, resources)?;
            column
                .try_push(cell)
                .map_err(|_| EngineRunError::allocation_failure())?;
        }
        columns
            .try_push(Value::Array(column))
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    Ok(Value::Array(columns))
}

/// The column count one child's `length` contributes.
///
/// Every length the reference produces is a non-negative count or a sign-stripped number, and only a count can widen
/// the result; a value too large to be a `usize` cannot be a container length either, so it contributes nothing.
fn column_count(number: &Number) -> usize {
    let raw = order::to_f64(number);
    if !raw.is_finite() || raw <= 0.0 {
        return 0;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the guard above admits only a positive finite count"
    )]
    let truncated = raw as u64;
    usize::try_from(truncated).unwrap_or(0)
}

/// Evaluates `has($key)` against a subject WITHOUT materializing it.
///
/// This is the family's one shallow read: the answer is a function of the container's KIND and of its member
/// identities, and of no payload below them.
/// A located object answers through the document's own key index, so `has("id")` over a large object is a lookup and
/// not a decode.
///
/// The presence law, not `.[k] != null`:
///
/// - an OBJECT admits a STRING key alone, and answers by PRESENCE — a member
///   holding `null` still answers `true`;
/// - an ARRAY admits a NUMBER key alone, truncates it toward zero and does NOT
///   wrap a negative one (`[1,2] | has(-1)` is `false`, though `.[-1]` is `2`);
/// - `null` answers `false` for ANY key, which is why `null | has("a")` is
///   `false` where `null | keys` raises;
/// - every other kind refuses.
///
/// # Errors
///
/// Returns the has-key refusal for a container/key pair the reference rejects, or an internal contract violation over a
/// malformed document.
pub fn has(input: &EngineResult<'_>, key: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let answer = match input {
        EngineResult::Located(located) => {
            // The payload-transparent view: a tag LAYER is descended before the node is asked its kind, so `has` over a
            // tagged container answers about the payload's member identities — the same law the owned arm spells for
            // `Value::Tagged`.
            let view = located
                .product()
                .document()
                .payload_view(located.node())
                .map_err(internal)?;
            located_has(&view, key, resources)?
        }
        EngineResult::Owned(owned) => owned_has(owned, key, resources)?,
    };
    Ok(Value::Bool(answer))
}

fn located_has(view: &ValueView<'_, '_>, key: &Value, resources: &ResourceContext<'_>) -> Result<bool, EngineRunError> {
    let kind = view.kind().map_err(internal)?;
    match (kind, key.untagged()) {
        (ValueKind::Object, Value::String(name)) => {
            let object = view
                .object()
                .map_err(internal)?
                .ok_or_else(|| EngineRunError::internal_contract("object node without an object projection"))?;
            Ok(object.get(name.as_str()).is_some())
        }
        (ValueKind::Array, Value::Number(number)) => {
            let array = view
                .array()
                .map_err(internal)?
                .ok_or_else(|| EngineRunError::internal_contract("array node without an array projection"))?;
            Ok(within(array.len(), number))
        }
        (ValueKind::Null, _) => Ok(false),
        (kind, component) => Err(refuse_has(kind, component, resources)),
    }
}

fn owned_has(input: &Value, key: &Value, resources: &ResourceContext<'_>) -> Result<bool, EngineRunError> {
    match (input.untagged(), key.untagged()) {
        (Value::Object(object), Value::String(name)) => Ok(object.get(name.as_str()).is_some()),
        (Value::Array(array), Value::Number(number)) => Ok(within(array.len(), number)),
        (Value::Null, _) => Ok(false),
        (container, component) => Err(refuse_has(container.kind(), component, resources)),
    }
}

/// The array membership test: truncate toward zero, then test `[0, len)`
/// WITHOUT the negative wrap `.[n]` applies.
fn within(len: usize, number: &Number) -> bool {
    let raw = order::to_f64(number);
    if !raw.is_finite() {
        return false;
    }
    let truncated = crate::semantics::arith::trunc_toward_zero(raw);
    if truncated < 0.0 {
        return false;
    }
    let Ok(len) = u64::try_from(len) else {
        return false;
    };
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the guards above admit only a non-negative finite value"
    )]
    let index = if truncated >= 18_446_744_073_709_551_616.0 {
        u64::MAX
    } else {
        truncated as u64
    };
    index < len
}

/// The has-key refusal: `Cannot check whether <kind> has a <keykind> key`.
fn refuse_has(container: ValueKind, key: &Value, resources: &ResourceContext<'_>) -> EngineRunError {
    match message::has_key_message(container, key.kind()) {
        Ok(text) => raise(&text, resources),
        Err(error) => error,
    }
}

/// An integer literal value, for the two numeric constants this module compares against.
fn integer(value: i64) -> Value {
    Value::Number(Number::integer(jqf_data::Integer::from_i64(value)))
}

/// The depth-limit refusal a comparison too deep to complete collapses into.
fn depth_too_deep() -> EngineRunError {
    EngineRunError::internal_contract("a flatten depth comparison exceeded the value depth limit")
}

/// A projection failure over an already-valid located document: an internal contract violation, never a program-visible
/// error.
fn internal(_: jqf_data::DataError) -> EngineRunError {
    EngineRunError::internal_contract("has evaluation over a valid located document failed")
}

const ADD_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::ADD),
    canonical_name: "add",
    category: "reshape",
    summary: "The `+` fold over a container's children, or over a filter's outputs.",
    detail: "Exactly `reduce .[] as $x (null; . + $x)`, so the seed is `null` \
             and the operator is the ordinary `+`: an empty input answers \
             `null`, an object folds its VALUES, strings concatenate, arrays \
             concatenate and objects merge. A mixed input raises `+`'s own type \
             error.",
};

const ADD_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::ADD),
    family: BuiltinFamilyId::new(id::ADD),
    canonical_name: "add",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "add",
            input: "[1,2,3]",
            expected: "6\n",
        },
        BuiltinExample {
            program: "add",
            input: "[]",
            expected: "null\n",
        },
    ],
};

const ADD_ONE_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::ADD_ONE),
    family: BuiltinFamilyId::new(id::ADD),
    canonical_name: "add",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Lowering,
    demand_transfer: DemandTransfer::ViaLowering,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "add(.[] | .n)",
            input: "[{\"n\":1},{\"n\":2}]",
            expected: "3\n",
        },
        BuiltinExample {
            program: "add(empty)",
            input: "null",
            expected: "null\n",
        },
    ],
};

const FLATTEN_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::FLATTEN),
    canonical_name: "flatten",
    category: "reshape",
    summary: "A container's children with nested ARRAYS spliced in, to a depth.",
    detail: "The arity-0 form has no depth limit. The countdown stops at depth \
             ZERO, so a FRACTIONAL depth steps past it and flattens all the way \
             down — `flatten(0.5)` is deeper than `flatten(2)`. A depth is spent \
             only when an array element is actually descended, so a non-numeric \
             depth is harmless over a flat array and raises `-`'s type error \
             over a nested one; a negative depth is refused up front, under the reference's \
             TOTAL order. Only ARRAY elements are spliced — an object nested \
             inside an array is a leaf, though an object at the TOP is iterated \
             for its values.",
};

const FLATTEN_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::FLATTEN),
    family: BuiltinFamilyId::new(id::FLATTEN),
    canonical_name: "flatten",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "flatten",
            input: "[1,[2,[3]]]",
            expected: "[1,2,3]\n",
        },
        BuiltinExample {
            program: "flatten",
            input: "[[],[[]]]",
            expected: "[]\n",
        },
    ],
};

const FLATTEN_ONE_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::FLATTEN_ONE),
    family: BuiltinFamilyId::new(id::FLATTEN),
    canonical_name: "flatten",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "flatten(1)",
            input: "[1,[2,[3]]]",
            expected: "[1,2,[3]]\n",
        },
        BuiltinExample {
            program: "[flatten(0,1)]",
            input: "[1,[2]]",
            expected: "[[1,[2]],[1,2]]\n",
        },
    ],
};

const TRANSPOSE_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::TRANSPOSE),
    canonical_name: "transpose",
    category: "reshape",
    summary: "An array of rows pivoted into an array of null-padded columns.",
    detail: "The column count is the LONGEST row's length, and short rows pad \
             with `null` rather than truncating. Every row is measured before \
             any row is indexed, so a row with no length is a length error and \
             never an index error.",
};

const TRANSPOSE_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::TRANSPOSE),
    family: BuiltinFamilyId::new(id::TRANSPOSE),
    canonical_name: "transpose",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "transpose",
            input: "[[1,2],[3,4,5]]",
            expected: "[[1,3],[2,4],[null,5]]\n",
        },
        BuiltinExample {
            program: "transpose",
            input: "[]",
            expected: "[]\n",
        },
    ],
};

const HAS_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::HAS),
    canonical_name: "has",
    category: "reshape",
    summary: "Whether a container carries a key, by PRESENCE and not by value.",
    detail: "An object takes a string key and an array a number key; the pairing \
             is strict, and a mismatched pair is refused rather than answered \
             `false`. `null` answers `false` for any key. A member holding \
             `null` still answers `true`, which is the whole difference from \
             `.[k] != null`. An array index truncates toward zero and does NOT \
             wrap a negative one.",
};

const HAS_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::HAS),
    family: BuiltinFamilyId::new(id::HAS),
    canonical_name: "has",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "has(\"a\")",
            input: "{\"a\":null}",
            expected: "true\n",
        },
        BuiltinExample {
            program: "has(1)",
            input: "[1,2]",
            expected: "true\n",
        },
    ],
};

const IN_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::IN_KEY),
    canonical_name: "in",
    category: "reshape",
    summary: "`has`, with the container and the key swapped.",
    detail: "Exactly `. as $x | xs | has($x)`: the INPUT is the key and the \
             argument is the container, so `\"a\" | in({\"a\":1})` is the same \
             question as `{\"a\":1} | has(\"a\")`. A multi-output argument \
             answers once per container.",
};

const IN_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::IN_KEY),
    family: BuiltinFamilyId::new(id::IN_KEY),
    canonical_name: "in",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Lowering,
    demand_transfer: DemandTransfer::ViaLowering,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "in({\"foo\":1})",
            input: "\"foo\"",
            expected: "true\n",
        },
        BuiltinExample {
            program: "[.[] | in([1,2])]",
            input: "[0,5]",
            expected: "[true,false]\n",
        },
    ],
};

const WALK_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::WALK),
    canonical_name: "walk",
    category: "reshape",
    summary: "A filter applied to every value of a document, BOTTOM-UP.",
    detail: "The reference's definition is `def w: if type == \"object\" then \
             map_values(w) elif type == \"array\" then map(w) else . end | f; \
             w`, and the two rebuild arms differ: an ARRAY collects every output \
             of its children's walks, while an OBJECT keeps the FIRST output per \
             key and DELETES the key when there is none. The filter runs at \
             every level, including the root, and may fan out there.",
};

const WALK_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::WALK),
    family: BuiltinFamilyId::new(id::WALK),
    canonical_name: "walk",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "walk(if type == \"array\" then sort else . end)",
            input: "[[3,1],[2]]",
            expected: "[[1,3],[2]]\n",
        },
        BuiltinExample {
            program: "walk(if type == \"number\" then . + 1 else . end)",
            input: "{\"a\":[1]}",
            expected: "{\"a\":[2]}\n",
        },
    ],
};

const MAP_VALUES_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::MAP_VALUES),
    canonical_name: "map_values",
    category: "reshape",
    summary: "A container rebuilt with a filter applied to each child IN PLACE.",
    detail: "Exactly `.[] |= f`, which is what distinguishes it from `map(f)`: \
             an OBJECT stays an object, only the FIRST output of `f` lands, and \
             a child for which `f` produces nothing is DELETED rather than \
             replaced by null.",
};

const MAP_VALUES_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::MAP_VALUES),
    family: BuiltinFamilyId::new(id::MAP_VALUES),
    canonical_name: "map_values",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "map_values(. + 1)",
            input: "{\"a\":1}",
            expected: "{\"a\":2}\n",
        },
        BuiltinExample {
            program: "map_values(empty)",
            input: "{\"a\":1,\"b\":2}",
            expected: "{}\n",
        },
        // A MULTI-VALUED update filter keeps the FIRST output per key (the `.[] |= f` law), the mirror of
        // `with_entries`, which keeps the last.
        BuiltinExample {
            program: "map_values(. + 10, . + 100)",
            input: "{\"a\":1,\"b\":2}",
            expected: "{\"a\":11,\"b\":12}\n",
        },
    ],
};

const PICK_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::PICK),
    canonical_name: "pick",
    category: "reshape",
    summary: "A document reduced to the paths a path expression names.",
    detail: "Exactly `. as $top | reduce path(pathexps) as $p (null; \
             setpath($p; $top | getpath($p)))`, so the skeleton is REBUILT from \
             `null`: `pick(empty)` answers `null`, an array path yields an array \
             padded with nulls, and a path the input cannot address raises \
             `path`'s own index error.",
};

const PICK_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::PICK),
    family: BuiltinFamilyId::new(id::PICK),
    canonical_name: "pick",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Lowering,
    demand_transfer: DemandTransfer::ViaLowering,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "pick(.a, .c)",
            input: "{\"a\":1,\"b\":2,\"c\":3}",
            expected: "{\"a\":1,\"c\":3}\n",
        },
        BuiltinExample {
            program: "pick(.[0])",
            input: "[1,2,3]",
            expected: "[1]\n",
        },
    ],
};

const IN_STREAM_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::IN_STREAM),
    canonical_name: "IN",
    category: "reshape",
    summary: "Whether a value occurs among a generator's outputs.",
    detail: "Exactly `any(s == .; .)` — and `any(s == src; .)` for the two-\
             argument form — so it SHORT-CIRCUITS: a generator that raises after \
             the match is never reached, an empty generator answers `false`, and \
             the answer is one boolean however many outputs the generator has.",
};

const IN_STREAM_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::IN_STREAM),
    family: BuiltinFamilyId::new(id::IN_STREAM),
    canonical_name: "IN",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Lowering,
    demand_transfer: DemandTransfer::ViaLowering,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "IN(1,2)",
            input: "1",
            expected: "true\n",
        },
        BuiltinExample {
            program: "IN(empty)",
            input: "1",
            expected: "false\n",
        },
    ],
};

const IN_STREAM_TWO_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::IN_STREAM_TWO),
    family: BuiltinFamilyId::new(id::IN_STREAM),
    canonical_name: "IN",
    arity: 2,
    parameters: &[ParameterKind::Filter, ParameterKind::Filter],
    execution: BuiltinExecution::Lowering,
    demand_transfer: DemandTransfer::ViaLowering,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "IN(.[]; 2)",
            input: "[1,2,3]",
            expected: "true\n",
        },
        BuiltinExample {
            program: "IN(.[]; 9)",
            input: "[1,2,3]",
            expected: "false\n",
        },
    ],
};

const INDEX_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::INDEX),
    canonical_name: "INDEX",
    category: "reshape",
    summary: "A stream re-keyed into one object by a key expression.",
    detail: "Exactly `reduce stream as $row ({}; .[$row|idx_expr|tostring] = \
             $row)`. The key is always STRINGIFIED, so a numeric id becomes a \
             numeric-looking key; a duplicate key keeps the LAST row at the \
             FIRST occurrence's position; a key expression with several outputs \
             files the row under every one of them, and one with none files it \
             nowhere.",
};

const INDEX_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::INDEX),
    family: BuiltinFamilyId::new(id::INDEX),
    canonical_name: "INDEX",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Lowering,
    demand_transfer: DemandTransfer::ViaLowering,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "INDEX(.id)",
            input: "[{\"id\":\"a\"},{\"id\":\"b\"}]",
            expected: "{\"a\":{\"id\":\"a\"},\"b\":{\"id\":\"b\"}}\n",
        },
        BuiltinExample {
            program: "INDEX(empty)",
            input: "[{\"id\":\"a\"}]",
            expected: "{}\n",
        },
        // A MULTI-VALUED index expression files each element under EVERY key it produces.
        BuiltinExample {
            program: "INDEX(.i, .j)",
            input: "[{\"i\":1,\"j\":9}]",
            expected: "{\"1\":{\"i\":1,\"j\":9},\"9\":{\"i\":1,\"j\":9}}\n",
        },
    ],
};

const INDEX_TWO_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::INDEX_TWO),
    family: BuiltinFamilyId::new(id::INDEX),
    canonical_name: "INDEX",
    arity: 2,
    parameters: &[ParameterKind::Filter, ParameterKind::Filter],
    execution: BuiltinExecution::Lowering,
    demand_transfer: DemandTransfer::ViaLowering,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "INDEX(.[]; .id)",
            input: "[{\"id\":\"a\"}]",
            expected: "{\"a\":{\"id\":\"a\"}}\n",
        },
        BuiltinExample {
            program: "INDEX(range(3); tostring)",
            input: "null",
            expected: "{\"0\":0,\"1\":1,\"2\":2}\n",
        },
    ],
};

const JOIN_INDEXED_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::JOIN_INDEXED),
    canonical_name: "JOIN",
    category: "reshape",
    summary: "A stream paired with the lookups an index object answers for it.",
    detail: "Each element becomes `[element, lookup]`, with a MISSING key \
             answering `null` rather than dropping the element — this is a left \
             join. The arity-2 form collects the pairs into one array; the \
             arity-3 form streams them; the arity-4 form pipes each pair through \
             a final filter. A key expression with several outputs widens the \
             pair rather than repeating it.",
};

const JOIN_TWO_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::JOIN_TWO),
    family: BuiltinFamilyId::new(id::JOIN_INDEXED),
    canonical_name: "JOIN",
    arity: 2,
    parameters: &[ParameterKind::Filter, ParameterKind::Filter],
    execution: BuiltinExecution::Lowering,
    demand_transfer: DemandTransfer::ViaLowering,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "JOIN({\"a\":1}; .k)",
            input: "[{\"k\":\"a\"},{\"k\":\"z\"}]",
            expected: "[[{\"k\":\"a\"},1],[{\"k\":\"z\"},null]]\n",
        },
        BuiltinExample {
            program: "JOIN({\"a\":1}; .k)",
            input: "[]",
            expected: "[]\n",
        },
    ],
};

const JOIN_THREE_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::JOIN_THREE),
    family: BuiltinFamilyId::new(id::JOIN_INDEXED),
    canonical_name: "JOIN",
    arity: 3,
    parameters: &[ParameterKind::Filter, ParameterKind::Filter, ParameterKind::Filter],
    execution: BuiltinExecution::Lowering,
    demand_transfer: DemandTransfer::ViaLowering,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "JOIN({\"a\":1}; .[]; .k)",
            input: "[{\"k\":\"a\"}]",
            expected: "[{\"k\":\"a\"},1]\n",
        },
        BuiltinExample {
            program: "[JOIN({\"a\":1}; .[]; .k)]",
            input: "[{\"k\":\"z\"}]",
            expected: "[[{\"k\":\"z\"},null]]\n",
        },
    ],
};

const JOIN_FOUR_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::JOIN_FOUR),
    family: BuiltinFamilyId::new(id::JOIN_INDEXED),
    canonical_name: "JOIN",
    arity: 4,
    parameters: &[
        ParameterKind::Filter,
        ParameterKind::Filter,
        ParameterKind::Filter,
        ParameterKind::Filter,
    ],
    execution: BuiltinExecution::Lowering,
    demand_transfer: DemandTransfer::ViaLowering,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "JOIN({\"a\":1}; .[]; .k; .[1])",
            input: "[{\"k\":\"a\"}]",
            expected: "1\n",
        },
        BuiltinExample {
            program: "[JOIN({\"a\":1}; .[]; .k; add)]",
            input: "[]",
            expected: "[]\n",
        },
    ],
};

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceLimits, WorkMeter};

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(1).expect("work"),
        )
        .expect("resources")
    }

    /// A FRACTIONAL depth never lands on zero: the countdown steps straight past it (`0.5 → -0.5 → -1.5 …`), so
    /// `flatten(0.5)` flattens ALL the way down while an integer depth stops where its countdown lands — the row the
    /// family record's detail names as deeper than any finite integer depth.
    #[test]
    fn flatten_fractional_depth_flattens_all_the_way_down() {
        let r = resources();
        let nested = |levels: usize| {
            let mut value = integer(1);
            for _ in 0..levels {
                let mut array = Array::try_with_capacity(1).expect("array");
                array.try_push(value).expect("push");
                value = Value::Array(array);
            }
            value
        };
        let decimal = |spelling: &str| {
            Value::Number(Number::decimal(
                jqf_data::Decimal::parse(spelling).expect("decimal spelling"),
            ))
        };

        // 0.5 spends past zero every level, so even a deep nest flattens fully down to its lone leaf.
        for levels in [5usize, 40] {
            match flatten(&nested(levels), Some(&decimal("0.5")), &r).expect("flatten") {
                Value::Array(array) => {
                    assert_eq!(array.len(), 1);
                    assert!(matches!(
                        array.get(0).map(jqf_data::Value::untagged),
                        Some(Value::Number(_))
                    ));
                }
                other => panic!("expected an array, got {other:?}"),
            }
        }

        // The sibling law on the same shape: integer depth 2 stops after two descents, leaving exactly three arrays
        // still around the leaf.
        let stopped = flatten(&nested(5), Some(&integer(2)), &r).expect("flatten");
        let mut node = stopped.untagged();
        for _ in 0..3 {
            let Value::Array(array) = node else {
                panic!("expected a nested array, got {node:?}")
            };
            assert_eq!(array.len(), 1);
            node = array.get(0).expect("nested element").untagged();
        }
        match node {
            Value::Number(number) => assert_eq!(number.to_i64(), Some(1)),
            other => panic!("expected the leaf number, got {other:?}"),
        }
    }
}
