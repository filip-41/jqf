//! The OWNED-value step navigation: the value helpers the step walk and the builtin registry share.
//!
//! These are the correction's "value helpers whose home is semantics": the owned index step (`index_owned`), the owned
//! key/index navigation it drives (`navigate_owned_key`/`navigate_owned_index`/`unrepresentable_index`), the
//! dynamic-position law (`DynAccess`/`dyn_index`), the payload-transparent kind probe (`owned_kind`), the slice bound
//! law (`resolve_slice_bound`), and the shared-value clone (`clone_owned`). The executor's step walk imports them from
//! here; nothing here touches the executor.

use jqf_data::{Value, ValueKind};
use jqf_resource::ResourceContext;

use crate::error::EngineRunError;
use crate::error::message;
use crate::error::mismatch;

/// One object key / array index step's resolution over a borrowed owned value.
pub enum OwnedNav<'value> {
    /// The step resolved to a borrowed child of the slot value.
    Value(&'value Value),
    /// Null precedence (missing key, out-of-bounds index, indexing into null).
    Null,
    /// The step addressed the wrong non-null semantic category.
    Mismatch(ValueKind),
}

/// Navigates one object key over a borrowed owned value.
///
/// Payload-transparent, exactly as the located route is: a tagged object IS an object and `.a` reads its payload's
/// member. The Mismatch arm therefore names the payload's own category — `owned_kind` sees through the wrapper, so a
/// tagged object's refusal says `Cannot index OBJECT with string`, the category the step declined to treat as a
/// container.
pub fn navigate_owned_key<'value>(
    value: &'value Value,
    key: &str,
    suppressed: bool,
    resources: &ResourceContext<'_>,
) -> Result<OwnedNav<'value>, EngineRunError> {
    match value.untagged() {
        Value::Null => mismatch::resolve_at(
            resources,
            mismatch::MismatchCell::FieldOnNull,
            suppressed,
            OwnedNav::Null,
        ),
        Value::Object(object) => match object.get(key) {
            Some(child) => Ok(OwnedNav::Value(child)),
            None => mismatch::resolve_at(
                resources,
                mismatch::MismatchCell::MissingKey,
                suppressed,
                OwnedNav::Null,
            ),
        },
        other => Ok(OwnedNav::Mismatch(owned_kind(other))),
    }
}

/// Navigates one signed array index over a borrowed owned value.
pub fn navigate_owned_index<'value>(
    value: &'value Value,
    index: i64,
    suppressed: bool,
    resources: &ResourceContext<'_>,
) -> Result<OwnedNav<'value>, EngineRunError> {
    match value.untagged() {
        Value::Null => mismatch::resolve_at(
            resources,
            mismatch::MismatchCell::IndexOrSliceOnNull,
            suppressed,
            OwnedNav::Null,
        ),
        Value::Array(array) => {
            match jqf_data::resolve_index(array.len(), index).and_then(|position| array.get(position)) {
                Some(child) => Ok(OwnedNav::Value(child)),
                None => mismatch::resolve_at(
                    resources,
                    mismatch::MismatchCell::IndexOutOfRange,
                    suppressed,
                    OwnedNav::Null,
                ),
            }
        }
        other => Ok(OwnedNav::Mismatch(owned_kind(other))),
    }
}

/// The answer for a numeric index no `i64` carries — a NaN, either infinity, or a magnitude past 2^63.
///
/// Such a position is outside every ARRAY's bounds, so an array answers `null` exactly as an ordinary out-of-bounds
/// index does, and so does `null` itself.
/// But being unrepresentable does not exempt the index from the CONTAINER-KIND law: every other container raises just
/// as it does for a representable position, so `{"a":1} | .[nan]` is `Cannot index object with number (null)` and `"ab"
/// | .[infinite]` is the string form.
/// The rejection is TOTAL over positions: a non-array, non-null container raises for an unrepresentable index exactly
/// as for a representable one, so `.[nan]` and `getpath([nan])` raise the same class over the same container.
pub fn unrepresentable_index<'value>(
    value: &'value Value,
    suppressed: bool,
    resources: &ResourceContext<'_>,
) -> Result<OwnedNav<'value>, EngineRunError> {
    match value {
        Value::Null | Value::Array(_) => mismatch::resolve_at(
            resources,
            mismatch::MismatchCell::IndexOutOfRange,
            suppressed,
            OwnedNav::Null,
        ),
        other => Ok(OwnedNav::Mismatch(owned_kind(other))),
    }
}

/// The one `.[$x]` accessor dispatch over a bound VALUE's runtime type.
pub enum DynAccess<'env> {
    /// A bound string reads an object key — borrowed straight out of the slot's storage (document text for a located
    /// bind), never materialized.
    Key(&'env str),
    /// A bound number reads an array position.
    Index(i64),
    /// A bound number outside the addressable range: the null precedence, not a mismatch.
    OutOfRange,
    /// The slice-object key (`.[{"start":a,"end":b}]`): the object component, dispatched through the same law
    /// `get_component` implements for the path walk (the step and the path now agree).
    Slice(jqf_data::Object),
    /// The subsequence-search key (`.[[1,2]]`): the array component answers `indices(needle)`, the same law
    /// `get_component` implements (the step divergence lifted).
    Subsequence(jqf_data::Array),
    /// Every other bound type is the index-class mismatch.
    Mismatch,
}

/// The dynamic array position from a bound number's binary64 value: truncate toward ZERO (`1.7` → 1, `-1.7` → -1,
/// `-0.5` → 0), then take the signed position. A magnitude beyond `i64` is out of bounds.
pub fn dyn_index<'env>(value: f64) -> DynAccess<'env> {
    let truncated = crate::semantics::arith::trunc_toward_zero(value);
    if !truncated.is_finite() || truncated.abs() >= 9_223_372_036_854_775_808.0 {
        return DynAccess::OutOfRange;
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the magnitude bound above keeps the truncated value inside i64"
    )]
    DynAccess::Index(truncated as i64)
}

/// One owned value's share, detached from its source allocation.
///
/// An owned `Value`'s `clone` is itself a SHARE on every heap-backed variant — elements, entries, string, bytes, tag
/// payload — rather than a deep copy.
/// There is one mechanism here, not two: an owned re-derivation is refcount bumps at the handle, and it is sound
/// because every mutator routes through a `try_*_mut` that detaches a non-unique allocation before writing, so a shared
/// re-derivation is observationally identical to a detached one.
/// (The deep copy this used to be was QUADRATIC on the descent.)
pub fn clone_owned(value: &Value) -> Value {
    value.clone()
}

/// The payload-transparent kind category of one owned value.
///
/// A tagged value reports its PAYLOAD's category (`Value::kind()` sees through the wrapper), so a refusal over a tagged
/// object says `OBJECT`, the category the step declined to treat as a container.
pub fn owned_kind(value: &Value) -> ValueKind {
    value.kind()
}

/// Resolves one authored slice bound against a container length: negative counts from the end, start rounds DOWN, end
/// rounds UP, then both clamp.
pub fn resolve_slice_bound(value: f64, len: usize, start: bool) -> usize {
    #[allow(
        clippy::cast_precision_loss,
        reason = "a container longer than 2^53 elements cannot exist in a decoded document; the \
                  binary64 length is the same projection the slice law uses"
    )]
    let length = len as f64;
    let relative = if value < 0.0 { length + value } else { value };
    let rounded = if start {
        crate::semantics::arith::floor_binary64(relative)
    } else {
        crate::semantics::arith::ceil_binary64(relative)
    };
    // The clamp also absorbs the infinities (a bound beyond `i64`, or `1e400` decoded as an infinity). A NaN bound
    // never reaches this point: the scalar-tails math stage made NaN constructible and pinned the law that it behaves
    // like an OPEN bound (`[range(3)] | .[1:nan]` is `[1,2]`), which `slice_bound` resolves before the position math
    // runs.
    if rounded
        .partial_cmp(&0.0)
        .is_none_or(|order| order != core::cmp::Ordering::Greater)
    {
        0
    } else if rounded >= length {
        len
    } else {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the branches above bound `rounded` to the open interval (0, len), and it is \
                      integral after rounding"
        )]
        let position = rounded as usize;
        position
    }
}

/// The `.[k]`/`.[i]` step law over an OWNED value — the `indices`/`index` fallthrough the registry reads without a
/// step.
pub fn index_owned(input: &Value, key: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let container = input.untagged();
    let nav = match key.untagged() {
        Value::String(text) => navigate_owned_key(container, text.as_str(), false, resources)?,
        Value::Number(number) => match dyn_index(crate::semantics::order::to_f64(number)) {
            DynAccess::Index(index) => navigate_owned_index(container, index, false, resources)?,
            _ => unrepresentable_index(container, false, resources)?,
        },
        // The slice-object key: `get_component`'s object arm, read from here so the owned step and the path walk share
        // one law.
        Value::Object(_) => return crate::semantics::path::get_component(input, key, resources),
        _ => OwnedNav::Mismatch(owned_kind(container)),
    };
    match nav {
        OwnedNav::Value(child) => Ok(clone_owned(child)),
        OwnedNav::Null => Ok(Value::Null),
        OwnedNav::Mismatch(kind) => {
            let rendered = message::render_owned_key(key)?;
            let text = message::index_message(kind, &rendered)?;
            Err(crate::semantics::path::raise(&text, resources))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_slice_bound;

    #[test]
    fn slice_bound_rounding_follows_the_len_relative_authored_sign_law() {
        // over a six-element array.
        let len = 6;
        // Integers, positive and negative.
        assert_eq!(resolve_slice_bound(1.0, len, true), 1);
        assert_eq!(resolve_slice_bound(-2.0, len, true), 4);
        assert_eq!(resolve_slice_bound(3.0, len, false), 3);
        assert_eq!(resolve_slice_bound(-1.0, len, false), 5);
        // Fractions round outward: floor for the start, ceil for the end.
        assert_eq!(resolve_slice_bound(1.7, len, true), 1);
        assert_eq!(resolve_slice_bound(3.2, len, false), 4);
        // Fractional NEGATIVES wrap FIRST, then round — `.[-1.5:]` is `[4,5]`, not element 5, because floor(6 - 1.5)
        // is 4 and not -2 + 6.
        assert_eq!(resolve_slice_bound(-1.5, len, true), 4);
        assert_eq!(resolve_slice_bound(-0.5, len, true), 5);
        // The discriminating row: `.[:-0.5]` is the WHOLE array. The end wraps to 5.5 and rounds UP to 6 — a naive
        // "ceil then wrap" would give 0.
        assert_eq!(resolve_slice_bound(-0.5, len, false), 6);
        assert_eq!(resolve_slice_bound(-0.2, len, false), 6);
        assert_eq!(resolve_slice_bound(-1.5, len, false), 5);
        // ... and `-0.0` is NOT negative, so `.[:-0.0]` is EMPTY.
        assert_eq!(resolve_slice_bound(-0.0, len, false), 0);
        assert_eq!(resolve_slice_bound(-0.0, len, true), 0);
        // Clamping absorbs everything out of range, in both directions.
        assert_eq!(resolve_slice_bound(99.0, len, true), 6);
        assert_eq!(resolve_slice_bound(-99.0, len, true), 0);
        assert_eq!(resolve_slice_bound(-6.5, len, false), 0);
        assert_eq!(resolve_slice_bound(6.5, len, false), 6);
    }

    #[test]
    fn slice_bound_rounding_clamps_the_i64_extremes_and_beyond() {
        let len = 3;
        assert_eq!(resolve_slice_bound(9_223_372_036_854_775_807.0, len, true), 3);
        assert_eq!(resolve_slice_bound(-9_223_372_036_854_775_808.0, len, true), 0);
        assert_eq!(resolve_slice_bound(9_223_372_036_854_775_807.0, len, false), 3);
        // Beyond `i64` entirely, including the infinities a `1e400` literal decodes to: the clamp still terminates at
        // the container's ends.
        assert_eq!(resolve_slice_bound(1e300, len, true), 3);
        assert_eq!(resolve_slice_bound(f64::INFINITY, len, false), 3);
        assert_eq!(resolve_slice_bound(f64::NEG_INFINITY, len, true), 0);
        assert_eq!(resolve_slice_bound(f64::NEG_INFINITY, len, false), 0);
        // NaN fails every comparison and lands at zero. No JSON document and no jqf literal can produce it (the
        // reference answer here is the whole array); it is recorded, never corpus-pinned.
        assert_eq!(resolve_slice_bound(f64::NAN, len, true), 0);
    }
}
