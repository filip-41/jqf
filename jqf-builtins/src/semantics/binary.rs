//! The value-level binary operator law: type dispatch, overloads, and errors.
//!
//! One job: apply one of jqf's eleven owned binary operators (`+ - * / %`, `== != < <= > >=`) to two owned operands,
//! producing an owned result value or a typed error. Numeric operands route to the exact-number engine
//! ([`crate::semantics::arith`]); comparisons route to the one total order ([`crate::semantics::order`]) and never
//! error. The non-numeric overloads:
//! `+` concatenates strings/arrays and right-biases object merge (with `null` as the additive identity), `-` removes
//! every `SemanticEq` occurrence for array difference, `*` repeats strings and deep-merges objects, and `/` splits a
//! string. Every operand pairing outside those overloads is one of the typed `cannot be
//! added/subtracted/multiplied/divided` classes.
//!
//! Negative space: it neither reads located documents nor drives cardinality — the executor materializes operands at
//! the op barrier and routes the result; it renders nothing (number bytes stay the codec's) and it does not own
//! `and`/`or`/`//`, which belong to other verticals.

use alloc::string::String;
use core::cmp::Ordering;

use jqf_data::{Array, Number, ObjectBuilder, Value, ValueKind};
use jqf_resource::ResourceContext;

use super::arith::{self, ArithOp, NumError, compute_number, trunc_toward_zero};
use super::depth::{Guarded, TooDeep, equality_message, message};
use super::order::{self, observable_cmp, semantic_eq};
use super::text;

/// One of jqf's eleven owned binary operators — the five arithmetic operators and the six comparisons. It is exactly
/// the operator surface this vertical owns: `and`/`or`/`//` are control-flow's, so they are absent here by
/// construction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BinaryKind {
    /// `+`
    Add,
    /// `-`
    Subtract,
    /// `*`
    Multiply,
    /// `/`
    Divide,
    /// `%`
    Remainder,
    /// `==`
    Equal,
    /// `!=`
    NotEqual,
    /// `<`
    Less,
    /// `<=`
    LessEqual,
    /// `>`
    Greater,
    /// `>=`
    GreaterEqual,
}

/// A typed failure applying a binary operator to two operands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BinaryError {
    /// An operand-type mismatch for `+ - * /` (the "X and Y cannot be added/subtracted/multiplied/divided" family).
    TypeMismatch {
        /// Which operator's mismatch template applies.
        op: MismatchOp,
        /// Payload-transparent category of the left operand.
        left: ValueKind,
        /// Payload-transparent category of the right operand.
        right: ValueKind,
    },
    /// `/` by a zero divisor.
    DivideByZero,
    /// `%` whose divisor truncates to zero.
    RemainderByZero,
    /// A numeric result left the representable range on a DEFENSIVE exact-path arm (see
    /// [`crate::semantics::arith::NumError::NumericRange`]); the binary64 path never raises it — a non-finite float
    /// result is a value like any other.
    NumericRange,
    /// A [`crate::semantics::depth`] cap tripped, ALREADY spelled for the operator that tripped it: the operation is
    /// named, so `==` says `Equality check too deep`, `<` says `Comparison too deep`, and `*` says `Object merge too
    /// deep`. The guard hands back a bare marker and this is where the three operator families become three messages.
    TooDeep(&'static str),
    /// String repetition would exceed the request's output-byte ceiling. A SEMANTIC class (the catch-eligible "Repeat
    /// string result too long"), deliberately raised before the ledger can refuse, so a `catch` written
    RepeatTooLong,
    /// Allocating the owned result failed.
    Allocation,
    /// A strict-dial mismatch cell fired at this operator: the additive law was about to absorb a null the request's
    /// policy refuses.
    /// Carries the cell's frozen row index; the machine's `from_binary` rebuilds the semantic raise so it surfaces at
    /// the operator's position.
    MismatchRaised(u16),
}

/// The operator whose error template a [`BinaryError::TypeMismatch`] names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MismatchOp {
    /// "cannot be added"
    Add,
    /// "cannot be subtracted"
    Subtract,
    /// "cannot be multiplied"
    Multiply,
    /// "cannot be divided"
    Divide,
    /// "cannot be divided (remainder)" — `%`'s own template, which names the remainder explicitly rather than reusing
    /// [`Self::Divide`]'s wording.
    Modulo,
}

/// Applies `op` to two owned operands.
pub fn apply(
    op: BinaryKind,
    left: &Value,
    right: &Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, BinaryError> {
    match op {
        BinaryKind::Add => add(left, right, resources),
        BinaryKind::Subtract => subtract(left, right, resources),
        BinaryKind::Multiply => multiply(left, right, resources),
        BinaryKind::Divide => divide(left, right, resources),
        BinaryKind::Remainder => remainder(left, right, resources),
        BinaryKind::Equal => Ok(Value::Bool(equality(left, right)?)),
        BinaryKind::NotEqual => Ok(Value::Bool(!equality(left, right)?)),
        BinaryKind::Less => Ok(Value::Bool(ordering(left, right)?.is_lt())),
        BinaryKind::LessEqual => Ok(Value::Bool(ordering(left, right)?.is_le())),
        BinaryKind::Greater => Ok(Value::Bool(ordering(left, right)?.is_gt())),
        BinaryKind::GreaterEqual => Ok(Value::Bool(ordering(left, right)?.is_ge())),
    }
}

/// Applies `op` to an OWNED left operand and a borrowed right one.
///
/// This is [`apply`]'s law with two additions, both of them the same idea: a fold hands the same accumulator back as
/// the left operand on every step, so the answer can be written INTO it rather than beside it.
///
/// For `+` over two containers or two STRINGS that means EXTENDING the left operand instead of building a third —
/// rebuilding it per step is O(n) per step and O(n²) over the fold, while extending is O(1) amortized when the
/// accumulator is the last handle on its allocation. For `+ - *` over two machine integers it means overwriting the
/// left operand's number representation instead of allocating a second one per step ([`arith_updating`]).
///
/// Both degrade to exactly what the general law would have done when the accumulator is NOT the last handle on its
/// allocation, because jqf-data's mutation paths detach a shared payload before they write.
///
/// The left operand comes BACK with a typed failure: the mismatch message renders both operands and the caller has no
/// other handle on this one.
pub fn apply_owned(
    op: BinaryKind,
    mut left: Value,
    right: &Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, (BinaryError, Value)> {
    if arith_updating(op, &mut left, right) {
        return Ok(left);
    }
    let left = match op {
        BinaryKind::Add => match add_extending(left, right, resources) {
            Extended::Whole(value) => return Ok(value),
            Extended::Raised(cell, left) => {
                return Err((BinaryError::MismatchRaised(cell), left));
            }
            Extended::Failed(error, left) => return Err((error, left)),
            Extended::Declined(left) => left,
        },
        _ => left,
    };
    match apply(op, &left, right, resources) {
        Ok(value) => Ok(value),
        Err(error) => Err((error, left)),
    }
}

/// `+ - *` over two numbers written as an UPDATE of the owned left operand, for the pairing whose answer fits a machine
/// integer.
///
/// `true` means `left` now HOLDS the result; `false` leaves it untouched for the general law, which computes the
/// identical number. The pairing this accepts is the intersection of two laws already written elsewhere, so it can add
/// no third one: [`arith::try_machine_integer_in_place`] owns which operand values and operators qualify, and the
/// operand shapes accepted here are exactly [`add`]'s, [`subtract`]'s and [`multiply`]'s numeric arms — an untagged
/// left number (a TAGGED one declines, because those arms answer with an UNTAGGED number and updating in place would
/// keep the tag) and a right number read through its tags.
///
/// `null` cannot reach here: it is not a number, so the additive identity stays [`add`]'s and [`add_extending`]'s to
/// answer.
fn arith_updating(op: BinaryKind, left: &mut Value, right: &Value) -> bool {
    let op = match op {
        BinaryKind::Add => ArithOp::Add,
        BinaryKind::Subtract => ArithOp::Subtract,
        BinaryKind::Multiply => ArithOp::Multiply,
        _ => return false,
    };
    let (Value::Number(target), Value::Number(source)) = (left, right.untagged()) else {
        return false;
    };
    arith::try_machine_integer_in_place(op, target, source) || arith::try_machine_decimal_in_place(op, target, source)
}

/// What [`add_extending`] made of the owned left operand.
enum Extended {
    /// The left operand absorbed the right and IS the whole result.
    Whole(Value),
    /// A strict-dial mismatch cell fired at the additive law; the raise travels verbatim to `apply_owned`'s error
    /// envelope, beside the left operand `add_extending` still holds.
    Raised(u16, Value),
    /// Not a pairing the left operand can absorb — the general law applies over the operand handed back untouched.
    Declined(Value),
    /// Growing the left operand failed. Only a ledger refusal or a failed allocation reaches here, never a type
    /// mismatch: a mismatched pairing DECLINES before anything is written, so no caller ever has to render a
    /// half-extended operand.
    Failed(BinaryError, Value),
}

/// `+` written as a mutation of the owned left operand, for the pairings where the answer is the left operand grown.
///
/// The string arm is [`concat_strings`]'s law written as an append.
/// `jqf-data`'s [`Shared::try_extend`](jqf_data::Shared::try_extend) is the builder, and it reserves ahead exactly so
/// this arm is amortized O(1) rather than one copy of the whole accumulated prefix per step.
///
/// A TAGGED left operand declines even over a matching payload, because [`add`]'s answer for one is an UNTAGGED
/// container and extending in place would keep the tag.
fn add_extending(mut left: Value, right: &Value, resources: &ResourceContext<'_>) -> Extended {
    // `x + null` IS `x`, for every `x` including `null` itself: the additive identity needs no copy at all. `null + x`
    // still copies `x` — there is nothing to extend — so it declines to the general law.
    if matches!(right.untagged(), Value::Null) {
        if crate::error::mismatch::resolve_at(
            resources,
            crate::error::mismatch::MismatchCell::NullAdditiveIdentity,
            false,
            (),
        )
        .is_err()
        {
            return Extended::Raised(
                u16::try_from(crate::error::mismatch::MismatchCell::NullAdditiveIdentity.index()).unwrap_or(u16::MAX),
                left,
            );
        }
        return Extended::Whole(left);
    }
    match (&mut left, right.untagged()) {
        (Value::String(target), Value::String(source)) => match target.try_extend(source.as_str()) {
            Ok(()) => Extended::Whole(left),
            Err(_) => Extended::Failed(BinaryError::Allocation, left),
        },
        (Value::Array(target), Value::Array(source)) => match extend_array(target, source, resources) {
            Ok(()) => Extended::Whole(left),
            Err(error) => Extended::Failed(error, left),
        },
        (Value::Object(target), Value::Object(source)) => match merge_into(target, source, resources) {
            Ok(()) => Extended::Whole(left),
            Err(error) => Extended::Failed(error, left),
        },
        _ => Extended::Declined(left),
    }
}

/// Appends the right operand's elements to the left's own spine.
///
/// Each element is a refcount bump, so the cost is the spine growth alone; the element ORDER and the element identities
/// are [`concat_arrays`]'s, written as a push instead of a rebuild. Two operands that name the same spine are safe by
/// construction: sharing makes the left one non-unique, so the first push detaches it onto its own allocation and the
/// right one still reads the original.
fn extend_array(target: &mut Array, source: &Array, _resources: &ResourceContext<'_>) -> Result<(), BinaryError> {
    target.try_extend_from(source).map_err(|_| BinaryError::Allocation)
}

/// Merges the right operand's entries into the left object.
///
/// [`shallow_merge`]'s law exactly — right-biased, an existing key keeping its FIRST-occurrence position while taking
/// the new value — written as a mutation of the left operand instead of a third table.
fn merge_into(
    target: &mut jqf_data::Object,
    source: &jqf_data::Object,
    _resources: &ResourceContext<'_>,
) -> Result<(), BinaryError> {
    for entry in source {
        let value = entry.value().clone();
        match target.key_position(entry.key()) {
            // The position was just resolved against this same table and a detach clones entries IN ORDER, so it still
            // names a slot; a `None` is unreachable and is answered as a refusal rather than a panic.
            Some(position) => {
                *target
                    .try_get_index_mut(position)
                    .map_err(|_| BinaryError::Allocation)?
                    .ok_or(BinaryError::Allocation)? = value;
            }
            None => {
                target
                    .try_insert_unique(entry.clone_key(), value)
                    .map_err(|_| BinaryError::Allocation)?;
            }
        }
    }
    Ok(())
}

/// The equality test, named the way `==`, `!=` and array difference name it.
///
/// Equality short-circuits on allocation identity BEFORE recursing, so `$x == $x` answers `true` at any depth; only two
/// SEPARATELY built values can reach the cap.
///
/// It is never true of a pair that reaches a NaN, which is why `[nan] - [nan]` keeps its element: array difference asks
/// this exact question.
fn equality(left: &Value, right: &Value) -> Result<bool, BinaryError> {
    semantic_eq(left, right).map_err(|TooDeep| BinaryError::TooDeep(equality_message()))
}

/// The comparison, named the way `<`, `<=`, `>` and `>=` name it. No identity short-circuit: an ordering caller must
/// answer `Ordering::Equal` for its own reasons, and the raise lands here where equality would have said `true`.
///
/// It reads the OBSERVABLE law, which is what makes the NaN operator table fall out of one rule: `nan < nan` and `nan
/// <= nan` are `true` while `nan > nan` and `nan >= nan` are `false`, because `observable_cmp(nan, nan)` is `Less`.
fn ordering(left: &Value, right: &Value) -> Result<Ordering, BinaryError> {
    observable_cmp(left, right).map_err(|TooDeep| BinaryError::TooDeep(message(Guarded::Comparison)))
}

fn add(left: &Value, right: &Value, resources: &ResourceContext<'_>) -> Result<Value, BinaryError> {
    // `null` is the additive identity in both positions (null + null -> null).
    // The dial's cell fires for either null operand — the additive law applied to a null IS the frozen `null + 1` /
    // `1
    // + null` row, whether the `+` is a program node, an update-law step, or the `add` builtin's own fold.
    if (matches!(left.untagged(), Value::Null) || matches!(right.untagged(), Value::Null))
        && crate::error::mismatch::resolve_at(
            resources,
            crate::error::mismatch::MismatchCell::NullAdditiveIdentity,
            false,
            (),
        )
        .is_err()
    {
        return Err(BinaryError::MismatchRaised(
            u16::try_from(crate::error::mismatch::MismatchCell::NullAdditiveIdentity.index()).unwrap_or(u16::MAX),
        ));
    }
    if matches!(left.untagged(), Value::Null) {
        return Ok(right.clone());
    }
    if matches!(right.untagged(), Value::Null) {
        return Ok(left.clone());
    }
    match (left.untagged(), right.untagged()) {
        (Value::Number(a), Value::Number(b)) => number_result(ArithOp::Add, a, b, resources),
        (Value::String(a), Value::String(b)) => concat_strings(a, b, resources),
        (Value::Array(a), Value::Array(b)) => concat_arrays(a, b, resources),
        (Value::Object(a), Value::Object(b)) => shallow_merge(a, b, resources),
        _ => Err(mismatch(MismatchOp::Add, left, right)),
    }
}

fn subtract(left: &Value, right: &Value, resources: &ResourceContext<'_>) -> Result<Value, BinaryError> {
    match (left.untagged(), right.untagged()) {
        (Value::Number(a), Value::Number(b)) => number_result(ArithOp::Subtract, a, b, resources),
        (Value::Array(a), Value::Array(b)) => array_difference(a, b, resources),
        _ => Err(mismatch(MismatchOp::Subtract, left, right)),
    }
}

fn multiply(left: &Value, right: &Value, resources: &ResourceContext<'_>) -> Result<Value, BinaryError> {
    match (left.untagged(), right.untagged()) {
        (Value::Number(a), Value::Number(b)) => number_result(ArithOp::Multiply, a, b, resources),
        (Value::String(text), Value::Number(count)) | (Value::Number(count), Value::String(text)) => {
            repeat_string(text, count, resources)
        }
        (Value::Object(a), Value::Object(b)) => deep_merge(a, b, 0),
        _ => Err(mismatch(MismatchOp::Multiply, left, right)),
    }
}

fn divide(left: &Value, right: &Value, resources: &ResourceContext<'_>) -> Result<Value, BinaryError> {
    match (left.untagged(), right.untagged()) {
        (Value::Number(a), Value::Number(b)) => number_result(ArithOp::Divide, a, b, resources),
        // The cut itself is `text::split`, because `split/1` is this same operator's definition (`def split($s):
        // ./$s;`) and one law cannot drift from itself.
        (Value::String(a), Value::String(b)) => text::split(a, b, resources).map_err(|_| BinaryError::Allocation),
        _ => Err(mismatch(MismatchOp::Divide, left, right)),
    }
}

fn remainder(left: &Value, right: &Value, resources: &ResourceContext<'_>) -> Result<Value, BinaryError> {
    match (left.untagged(), right.untagged()) {
        (Value::Number(a), Value::Number(b)) => number_result(ArithOp::Remainder, a, b, resources),
        _ => Err(mismatch(MismatchOp::Modulo, left, right)),
    }
}

/// Computes a numeric result and maps the number engine's errors onto the value-level error family.
fn number_result(
    op: ArithOp,
    left: &Number,
    right: &Number,
    resources: &ResourceContext<'_>,
) -> Result<Value, BinaryError> {
    match compute_number(op, left, right, resources) {
        Ok(number) => Ok(Value::Number(number)),
        Err(NumError::DivideByZero) => Err(BinaryError::DivideByZero),
        Err(NumError::RemainderByZero) => Err(BinaryError::RemainderByZero),
        Err(NumError::NumericRange) => Err(BinaryError::NumericRange),
        Err(NumError::Allocation) => Err(BinaryError::Allocation),
    }
}

fn concat_strings(left: &str, right: &str, _resources: &ResourceContext<'_>) -> Result<Value, BinaryError> {
    let mut text = String::new();
    text.try_reserve_exact(left.len() + right.len())
        .map_err(|_| BinaryError::Allocation)?;
    text.push_str(left);
    text.push_str(right);
    Value::try_string(&text).map_err(|_| BinaryError::Allocation)
}

fn concat_arrays(left: &Array, right: &Array, _resources: &ResourceContext<'_>) -> Result<Value, BinaryError> {
    let mut array = Array::try_with_capacity(left.len() + right.len()).map_err(|_| BinaryError::Allocation)?;
    for value in left.iter().chain(right.iter()) {
        array.try_push(value.clone()).map_err(|_| BinaryError::Allocation)?;
    }
    Ok(Value::Array(array))
}

/// Array difference: every left element with no `SemanticEq` occurrence in the right operand, cross-spelling (`[1] -
/// [1.0]` is empty).
fn array_difference(left: &Array, right: &Array, _resources: &ResourceContext<'_>) -> Result<Value, BinaryError> {
    let mut array = Array::try_new().map_err(|_| BinaryError::Allocation)?;
    for value in left {
        if !occurs_in(right, value)? {
            array.try_push(value.clone()).map_err(|_| BinaryError::Allocation)?;
        }
    }
    Ok(Value::Array(array))
}

/// Whether `needle` has a `SemanticEq` occurrence in `haystack`.
///
/// Written as a loop rather than `Iterator::any` because the equality test can now FAIL (the depth cap), and swallowing
/// that into a `false` would silently keep an element the equality refuses to compare.
fn occurs_in(haystack: &Array, needle: &Value) -> Result<bool, BinaryError> {
    for other in haystack {
        if equality(needle, other)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Shallow object merge (`+`): right-biased, first-occurrence position, last value wins — an `ObjectBuilder` pass
/// over left then right entries.
///
/// Every key in the result already exists in an operand, so the merged object RETAINS the operand's key allocation
/// instead of copying its text.
fn shallow_merge(
    left: &jqf_data::Object,
    right: &jqf_data::Object,
    _resources: &ResourceContext<'_>,
) -> Result<Value, BinaryError> {
    // Both operands are already unique-key objects, so the builder's entries stay unique by construction: left entries
    // first, right entries replacing in place at the left position when the key exists (the first-position/last-value
    // law), appended otherwise. Finishing through `try_finish_unique` skips the re-proving dedup pass entirely.
    let mut builder =
        ObjectBuilder::try_with_capacity(left.len() + right.len()).map_err(|_| BinaryError::Allocation)?;
    for entry in left {
        builder
            .try_insert_last(entry.clone_key(), entry.value().clone())
            .map_err(|_| BinaryError::Allocation)?;
    }
    for entry in right {
        if let Some(position) = left.key_position(entry.key()) {
            builder
                .replace_value_at(position, entry.value().clone())
                .ok_or(BinaryError::Allocation)?;
        } else {
            builder
                .try_insert_last(entry.clone_key(), entry.value().clone())
                .map_err(|_| BinaryError::Allocation)?;
        }
    }
    Ok(Value::Object(
        builder.try_finish_unique().map_err(|_| BinaryError::Allocation)?,
    ))
}

/// Deep object merge (`*`): recurse where both sides hold an object, otherwise the right value replaces (the recursive
/// merge; `null` replaces too, as it is not an object). Position follows the same first-occurrence law.
///
/// `depth` is 0 at the operator and rises once per nested pair of objects, so a 10000-deep pair merges and a 10001-deep
/// pair raises.
fn deep_merge(left: &jqf_data::Object, right: &jqf_data::Object, depth: usize) -> Result<Value, BinaryError> {
    if depth > super::depth::limit(Guarded::ObjectMerge) {
        return Err(BinaryError::TooDeep(message(Guarded::ObjectMerge)));
    }
    let mut builder =
        ObjectBuilder::try_with_capacity(left.len() + right.len()).map_err(|_| BinaryError::Allocation)?;
    for entry in left {
        builder
            .try_insert_last(entry.clone_key(), entry.value().clone())
            .map_err(|_| BinaryError::Allocation)?;
    }
    for entry in right {
        // One hash probe for the position, then an O(1) index read for the base object: the position answers both "does
        // the key exist" and "where" without a second table walk.
        let position = left.key_position(entry.key());
        let base = position.and_then(|index| left.get_index(index));
        let merged = match (base.map(jqf_data::ObjectEntry::value), entry.value()) {
            (Some(Value::Object(base)), Value::Object(overlay)) => deep_merge(base, overlay, depth + 1)?,
            (_, value) => value.clone(),
        };
        if let Some(position) = position {
            builder
                .replace_value_at(position, merged)
                .ok_or(BinaryError::Allocation)?;
        } else {
            builder
                .try_insert_last(entry.clone_key(), merged)
                .map_err(|_| BinaryError::Allocation)?;
        }
    }
    Ok(Value::Object(
        builder.try_finish_unique().map_err(|_| BinaryError::Allocation)?,
    ))
}
/// String repetition (`*`): the sign test reads the RAW count and only the total-bytes ceiling is the operator's own
/// law — a result past 2^31 − 10 bytes raises the catch-eligible "Repeat string result too long", independent of
/// any request output ceiling.
const MAX_REPEAT_BYTES: usize = (1 << 31) - 10;

fn repeat_string(text: &str, count: &Number, _resources: &ResourceContext<'_>) -> Result<Value, BinaryError> {
    let count = order::to_f64(count);

    if count.is_nan() {
        return Ok(Value::Null);
    }
    if count < 0.0 {
        return Ok(Value::Null);
    }
    // The count is read THROUGH BINARY64 first (`to_f64` above) and the sign test and truncation run on that double —
    // the reliance this function makes deliberately. String repetition is a double-arithmetic operation under the
    // reference's law, so an exact count past binary64's precision repeats by its ROUNDED double (a 2^53 + 1 count
    // repeats 2^53 times), never by re-parsing the exact digits.
    let repeats = usize_from_f64(trunc_toward_zero(count));

    let Some(total) = text.len().checked_mul(repeats) else {
        return Err(BinaryError::RepeatTooLong);
    };
    if total > MAX_REPEAT_BYTES {
        return Err(BinaryError::RepeatTooLong);
    }
    // The overflow refusal above already ran, so the built-in `repeat` cannot overflow.
    let out = text.repeat(repeats);
    Value::try_string(&out).map_err(|_| BinaryError::Allocation)
}

fn mismatch(op: MismatchOp, left: &Value, right: &Value) -> BinaryError {
    BinaryError::TypeMismatch {
        op,
        left: left.kind(),
        right: right.kind(),
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "the repeat count is clamped into the valid usize range before the cast"
)]
fn usize_from_f64(value: f64) -> usize {
    // The float->int `as` cast is the documented saturating cast (NaN->0, out-of-range clamps) since Rust 1.45 —
    // precisely the law the old branches re-spelled.
    value as usize
}

#[cfg(test)]
mod tests {
    use super::{BinaryError, MAX_REPEAT_BYTES, MismatchOp, apply as apply_binary, apply_owned, semantic_eq};
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use jqf_data::{Array, Number, ObjectBuilder, Value, ValueKind};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    use crate::semantics::binary::BinaryKind;

    static CONTROL: ContinueControl = ContinueControl;

    /// One unlimited request ledger: a binary result allocates its own payload, which is charged at construction, so
    /// every case needs an account.
    fn ledger() -> ResourceContext<'static> {
        let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let account = RequestAccount::try_new(limits).expect("test account");
        let work = WorkMeter::try_new_v1(1).expect("test work meter");
        ResourceContext::new(account, &CONTROL, work).expect("test ledger")
    }

    fn apply(op: BinaryKind, left: &Value, right: &Value) -> Result<Value, BinaryError> {
        apply_binary(op, left, right, &ledger())
    }

    fn number(spelling: &str) -> Value {
        Value::Number(Number::try_json_literal(spelling).expect("literal"))
    }

    fn string(text: &str) -> Value {
        Value::try_string(text).expect("string")
    }

    fn array(values: Vec<Value>) -> Value {
        Value::Array(Array::try_from_vec(values).expect("array"))
    }

    fn object(entries: Vec<(&str, Value)>) -> Value {
        let mut builder = ObjectBuilder::new();
        for (key, value) in entries {
            builder
                .try_insert_last(jqf_data::ObjectKey::try_from_str(key).expect("key"), value)
                .expect("insert");
        }
        Value::Object(builder.try_finish().expect("object"))
    }

    fn text_of(value: &Value) -> String {
        match value {
            Value::String(text) => String::from(text.as_str()),
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn addition_overloads_and_null_identity() {
        assert!(matches!(
            apply(BinaryKind::Add, &Value::Null, &Value::Null).expect("ok"),
            Value::Null
        ));
        assert_eq!(
            text_of(&apply(BinaryKind::Add, &string("a"), &string("b")).expect("ok")),
            "ab"
        );
        let concatenated = apply(BinaryKind::Add, &array(vec![number("1")]), &array(vec![number("2")])).expect("ok");
        assert!(matches!(concatenated, Value::Array(ref a) if a.len() == 2));
        // null + number is the number.
        assert!(matches!(
            apply(BinaryKind::Add, &Value::Null, &number("5")).expect("ok"),
            Value::Number(_)
        ));
    }

    /// `apply_owned` answers what `apply` answers for every numeric pairing — the whole point of the in-place door
    /// being invisible.
    ///
    /// The rows walk what [`arith_updating`] accepts and what it declines: the five operators, machine and
    /// beyond-machine operands, non-integer categories, `null` in either position, a TAGGED left operand (declines,
    /// because the answer must be untagged) and a tagged right one (accepted, exactly as the general arms read it).
    #[test]
    fn owned_application_answers_what_the_borrowed_one_answers() {
        let tag = jqf_data::TagId::try_new_unaccounted("t").expect("tag");
        let tagged = |value: Value| Value::try_tagged(tag.clone(), value).expect("tagged");
        let operands: Vec<Value> = vec![
            number("0"),
            number("41"),
            number("-41"),
            number("9223372036854775807"),
            number("9223372036854775808"),
            number("1.5"),
            number("1.000"),
            Value::Null,
            // A non-number with no numeric overload at all. A STRING would be the richer probe, but `*` repeats one by
            // a count, and the beyond-machine rows above would ask this matrix to build a string of `i64::MAX` bytes;
            // the string overloads are pinned by their own tests instead.
            Value::Bool(true),
            tagged(number("1")),
        ];
        let ops = [
            BinaryKind::Add,
            BinaryKind::Subtract,
            BinaryKind::Multiply,
            BinaryKind::Divide,
            BinaryKind::Remainder,
            BinaryKind::Equal,
            BinaryKind::Less,
        ];
        for op in ops {
            for left in &operands {
                for right in &operands {
                    let borrowed = apply(op, left, right);
                    let owned = apply_owned(op, left.clone(), right, &ledger());
                    match (borrowed, owned) {
                        (Ok(borrowed), Ok(owned)) => assert!(
                            semantic_eq(&borrowed, &owned).expect("shallow") && borrowed.kind() == owned.kind(),
                            "{op:?} {left:?} {right:?}: {borrowed:?} vs {owned:?}"
                        ),
                        (Err(borrowed), Err((owned, _))) => {
                            assert_eq!(borrowed, owned, "{op:?} {left:?} {right:?}");
                        }
                        (borrowed, owned) => panic!(
                            "{op:?} {left:?} {right:?} disagree: {borrowed:?} vs {:?}",
                            owned.map_err(|(error, _)| error)
                        ),
                    }
                }
            }
        }
    }

    #[test]
    fn addition_type_mismatch_is_typed() {
        assert_eq!(
            apply(BinaryKind::Add, &number("1"), &string("a")).unwrap_err(),
            BinaryError::TypeMismatch {
                op: MismatchOp::Add,
                left: ValueKind::Number,
                right: ValueKind::String,
            }
        );
    }

    #[test]
    fn multiply_string_repeat_commutes_and_bounds() {
        assert_eq!(
            text_of(&apply(BinaryKind::Multiply, &string("ab"), &number("3")).expect("ok")),
            "ababab"
        );
        assert_eq!(
            text_of(&apply(BinaryKind::Multiply, &number("3"), &string("ab")).expect("ok")),
            "ababab"
        );
        assert_eq!(
            text_of(&apply(BinaryKind::Multiply, &string("ab"), &number("0")).expect("ok")),
            ""
        );
        assert!(matches!(
            apply(BinaryKind::Multiply, &string("ab"), &number("-1")).expect("ok"),
            Value::Null
        ));
        // The sign test reads the RAW count: a fractional negative is null even though it truncates to zero, and
        // negative zero is not negative.
        assert!(matches!(
            apply(BinaryKind::Multiply, &string("ab"), &number("-0.5")).expect("ok"),
            Value::Null
        ));
        assert_eq!(
            text_of(&apply(BinaryKind::Multiply, &string("ab"), &number("-0.0")).expect("ok")),
            ""
        );
        assert_eq!(
            text_of(&apply(BinaryKind::Multiply, &string("ab"), &number("0.5")).expect("ok")),
            ""
        );
    }

    #[test]
    fn repeat_past_the_operators_own_byte_ceiling_is_a_semantic_error() {
        // The string-repeat law is the operator's own total-bytes ceiling (2^31 − 10), independent of any request
        // output ceiling: a count whose product overflows the ceiling raises the catch-eligible "Repeat string result
        // too long" (the boundary is pinned just PAST `MAX_REPEAT_BYTES`, refused, and AT it, successful).
        assert!(matches!(
            apply(BinaryKind::Multiply, &string("abc"), &number("18446744073709551615"))
                .expect_err("repeat must be refused"),
            BinaryError::RepeatTooLong
        ));
        assert!(matches!(
            apply(
                BinaryKind::Multiply,
                &string("a"),
                &number(&alloc::format!("{}", MAX_REPEAT_BYTES as u64 + 1))
            )
            .expect_err("repeat past the ceiling must be refused"),
            BinaryError::RepeatTooLong
        ));
        // At the ceiling the answer is SUCCESS, asserted by length alone:
        // materializing a second 2 GiB expected string just to compare bytes is the one thing this row must not do.
        let at_ceiling = apply(
            BinaryKind::Multiply,
            &string("a"),
            &number(&alloc::format!("{}", MAX_REPEAT_BYTES as u64)),
        )
        .expect("repeat at the ceiling is legal");
        assert!(matches!(
            at_ceiling,
            Value::String(ref repeated) if repeated.as_str().len() == MAX_REPEAT_BYTES
        ));
    }

    #[test]
    fn object_merges_shallow_and_deep() {
        let shallow = apply(
            BinaryKind::Add,
            &object(vec![("a", number("1"))]),
            &object(vec![("b", number("2"))]),
        )
        .expect("ok");
        assert!(matches!(shallow, Value::Object(ref o) if o.len() == 2));
        let deep = apply(
            BinaryKind::Multiply,
            &object(vec![("a", object(vec![("x", number("1"))]))]),
            &object(vec![("a", object(vec![("y", number("2"))]))]),
        )
        .expect("ok");
        let Value::Object(object) = deep else {
            panic!("expected object");
        };
        let Some(Value::Object(inner)) = object.get("a") else {
            panic!("expected nested object");
        };
        assert_eq!(inner.len(), 2);
    }

    #[test]
    fn array_difference_removes_cross_spelling_matches() {
        let difference = apply(
            BinaryKind::Subtract,
            &array(vec![number("1")]),
            &array(vec![number("1.0")]),
        )
        .expect("ok");
        assert!(matches!(difference, Value::Array(ref a) if a.is_empty()));
    }

    #[test]
    fn division_splits_strings_and_handles_edges() {
        let chars = apply(BinaryKind::Divide, &string("abc"), &string("")).expect("ok");
        assert!(matches!(chars, Value::Array(ref a) if a.len() == 3));
        let empty = apply(BinaryKind::Divide, &string(""), &string(",")).expect("ok");
        assert!(matches!(empty, Value::Array(ref a) if a.is_empty()));
        let split = apply(BinaryKind::Divide, &string("abc"), &string("abc")).expect("ok");
        assert!(matches!(split, Value::Array(ref a) if a.len() == 2));
    }

    #[test]
    fn comparisons_never_error_and_use_the_total_order() {
        assert!(matches!(
            apply(BinaryKind::Equal, &number("1"), &number("1.0")).expect("ok"),
            Value::Bool(true)
        ));
        assert!(matches!(
            apply(BinaryKind::Less, &Value::Null, &Value::Bool(false)).expect("ok"),
            Value::Bool(true)
        ));
    }
}
