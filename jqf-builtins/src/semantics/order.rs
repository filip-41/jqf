//! jqf's one total order and semantic equality over owned values.
//!
//! One job: rank two owned [`Value`]s under jqf's total order — `null < false < true < number < string < bytes <
//! localdate < localtime < localdatetime < offsetdatetime < array < object` — with numbers compared by exact
//! mathematical value (`1 == 1.0`, and `9007199254740993 != 9007199254740992` — literal comparisons stay exact),
//! strings by Unicode scalar (UTF-8 byte order is codepoint order), arrays lexicographically then by length, and
//! objects by their sorted key lists then by values in sorted-key order. [`semantic_eq`] is `observable_cmp == Equal`,
//! so the `==`/`!=`/`<`/`<=`/`>`/`>=` operators share one law. It is payload-transparent: a tagged value ranks by its
//! payload (JSON, this vertical's only surface, carries no tags).
//!
//! # The two NaN laws, and why there are two
//!
//! The observable rule: `nan == nan` is `false` and `nan < nan` is `true` while `nan >= nan` is `false`, `sort` places
//! NaN below every number, `unique`/`group_by` never merge two NaNs, `index(nan)` is `null`, and container equality is
//! nan-poisoned structurally (`{"a":nan} == {"a":nan}` is `false`).
//! The observable law [`NanLaw::Observable`] IS that rule.
//!
//! That rule is not a total order — `cmp(nan, nan)` is `Less` in BOTH argument orders — and Rust's `slice::sort_by`
//! has detected and panicked on inconsistent comparators since 1.81. So the INTERNAL order the sort machinery runs on
//! is [`NanLaw::Total`]: the same law with NaN as ONE point at the BOTTOM of the number band, `cmp(nan, nan) == Equal`.
//! The two differ at exactly one leaf — NaN against NaN — and nowhere else, which is what makes the layering sound:
//!
//! * SORT PLACEMENT and `min`/`max` fall out of NaN-below-every-number, which is
//!   the same fact under both laws;
//! * the OPERATORS, the `unique`/`group_by` run boundaries, `index`/`indices`,
//!   `contains`/`inside` and `bsearch` read the observable law, because their answers turn on precisely the
//!   NaN-against-NaN leaf.
//!
//! Dedup MUST read the observable law: a `unique` that split runs by the internal order would collapse `[nan,nan]` to
//! one element, where the answer is two.
//!
//! One recorded NARROWING. Under the observable rule `sort` has no consistent answer for two containers that TIE at a
//! NaN and differ after it, and its output there is a platform qsort artifact: `[[nan,5],[nan,3],[nan,8],[nan,1],
//! [nan,7],[nan,2],[nan,6],[nan,4]] | sort | map(.[1])` is `[4,3,8,1,5,2,6,7]` — neither input order nor sorted.
//! jqf's internal order answers such a tie by continuing to the next component, which is deterministic,
//! permutation-stable and a real total order. It is a catalogued intentional divergence, not an oversight; every case
//! where the observable answer is well defined agrees.
//!
//! # The beyond-compat kinds
//!
//! The compatibility order stops at objects. The four temporal kinds and `bytes` are the engine's extension, and
//! `docs/architecture/contracts.md` ("Central value laws") ALREADY specifies where they sit and how each one orders —
//! this module implements that document rather than inventing a second answer:
//!
//! * bytes order lexicographically by unsigned byte value;
//! * a local date orders by year, month, day; a local time by hour, minute,
//!   second and normalized fractional second (so the syntactic leap-second label `60` stays distinct from `00` of the
//!   next minute); a local date-time by date then time. Those are exactly the field orders `jqf-data`'s derived [`Ord`]
//!   gives, so the law is READ from the storage rather than restated;
//! * an offset date-time orders by its REPRESENTED INSTANT, so two spellings of
//!   one instant are equal (`…T12:00:00Z` and `…T14:00:00+02:00`), and the `-00:00` unknown-local marker has
//!   effective offset zero;
//! * the five kinds are five separate ranks, so values from different temporal
//!   categories are never equal (a `localdate` is never a `localdatetime`).
//!
//! One consequence is worth naming: instant comparison is MATHEMATICAL, and jqf carries no leap-second table, so an
//! offset date-time labelled `23:59:60` denotes the same instant as the following `00:00:00` and compares equal to it.
//! Adding a leap-second tiebreak would break the documented law that two spellings of one instant are equal, because an
//! offset with a sub-minute component can shift the second field between spellings.
//!
//! # The depth cap
//!
//! The recursion is bounded by [`super::depth::Guarded::Comparison`], which caps at `deep` — so the spelling belongs
//! to the caller.
//!
//! [`semantic_eq`] additionally short-circuits on ALLOCATION IDENTITY before it recurses at all, and [`total_cmp`]
//! deliberately does NOT. That asymmetry is observable: `$x == $x` at depth 10001 is `true` while `[$x,$x] | sort` at
//! the same depth raises. A shared guard with no identity test fails the first; one that adds the test to the ordering
//! side mis-sorts.
//!
//! Negative space: comparisons allocate no result and raise no message — they return an [`Ordering`] or the depth
//! marker; the boolean value, the message and their routing belong to [`crate::semantics::binary`] and [`crate::exec`].
//! It computes no arithmetic (that is [`crate::semantics::arith`]); numeric ordering is value law, not the exact-number
//! engine.

use alloc::vec::Vec;
use core::cmp::Ordering;

use jqf_data::{Number, NumberCategory, NumberView, OffsetDateTime, UtcOffset, Value};

use super::depth::{self, Guarded, TooDeep};
use jqf_data::BigInt;

/// Which NaN rule a comparison answers under. See the module note: the two laws differ at exactly one leaf, NaN against
/// NaN, and agree everywhere else.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NanLaw {
    /// The INTERNAL, sort-safe law: NaN is ONE point at the bottom of the number band and equal to itself. A real total
    /// order, so `slice::sort_by` cannot see the inconsistency it panics on.
    Total,

    Observable,
}

/// Ranks two owned values under jqf's one INTERNAL total order — the order every sort and every ordered index runs
/// on, because those are the consumers that need a CONSISTENT comparator.
///
/// # Errors
///
/// Returns [`TooDeep`] once the recursion passes the comparison row's ceiling.
/// The caller names the operation; this function never spells the message.
pub fn total_cmp(left: &Value, right: &Value) -> Result<Ordering, TooDeep> {
    compare(left, right, 0, NanLaw::Total)
}

/// Ranks two owned values under jqf's OBSERVABLE law — the law the comparison operators and the equality consumers
/// read, where `nan == nan` is `false` and `nan < nan` is `true`.
///
/// # Errors
///
/// Returns [`TooDeep`] once the recursion passes the comparison row's ceiling.
pub fn observable_cmp(left: &Value, right: &Value) -> Result<Ordering, TooDeep> {
    compare(left, right, 0, NanLaw::Observable)
}

/// Whether two owned values are observably equal under the `==` law, for callers OUTSIDE the engine (the engine crate's
/// SDK edit lane is the intended first caller once that crate lands: it must judge a span-less decoded leaf against the
/// program's output value without growing a second, divergent value law).
///
/// This is the public face of [`semantic_eq`]. It answers `false` — the safe, floor-ward answer — when the
/// comparison exceeds the depth ceiling, because the one caller that needs it (a scalar leaf) can never reach that
/// ceiling; a container-holding caller that needs the distinction should call the internal [`semantic_eq`] directly and
/// see [`TooDeep`].
#[must_use]
pub fn values_semantically_equal(left: &Value, right: &Value) -> bool {
    semantic_eq(left, right).unwrap_or(false)
}

/// Whether two owned values are semantically equal (`observable_cmp == Equal`).
///
/// This is the `==` law, so it is NEVER true of a pair that reaches a NaN:
/// `nan == nan` is `false`, and a container carrying one is poisoned structurally (`{"a":nan} == {"a":nan}` is `false`)
/// because the recursion stops at the NaN leaf with `Less` rather than continuing past it.
///
/// Two values sharing one allocation answer `true` before any walk — the allocation-identity short-circuit,
/// observable as `[x] as $x | $x == $x` answering `true` where `[x] == [x]` over two separate allocations is `false`.
///
/// # Errors
///
/// Returns [`TooDeep`] once the recursion passes the comparison row's ceiling.
pub fn semantic_eq(left: &Value, right: &Value) -> Result<bool, TooDeep> {
    if left.shares_allocation_with(right) {
        // The identity short-circuit covers allocated payloads — arrays, objects, strings, and boxed decimal numbers
        // — but NOT the native NaN doubles, whose equality falls to the number comparison:

        let shared_nan = matches!(
            left.untagged(),
            Value::Number(number)
                if number.as_float().is_some_and(|f| f.get().is_nan())
        );
        if !shared_nan {
            return Ok(true);
        }
    }
    Ok(observable_cmp(left, right)? == Ordering::Equal)
}

/// Whether `value` carries a NaN anywhere the total order would reach.
///
/// Read straight OFF the two laws rather than by a second structural walk: they agree everywhere except the
/// NaN-against-NaN leaf, so comparing a value with ITSELF answers `Equal` under the observable law exactly when the
/// walk reached no NaN — and it walks every position it would compare. It therefore inherits the depth guard for
/// free, and costs one comparison rather than a new recursion with its own stack hazard.
///
/// Its caller is the machinery whose soundness argument is "ordered by `total_cmp`, therefore equivalent to `==`" —
/// an argument that holds only for NaN-free values now that `==` reads the observable law.
///
/// # Errors
///
/// Returns [`TooDeep`] once the recursion passes the comparison row's ceiling, which is neither answer and is the
/// caller's own decline.
pub fn carries_nan(value: &Value) -> Result<bool, TooDeep> {
    Ok(observable_cmp(value, value)? != Ordering::Equal)
}

/// One level of the total order's recursion.
///
/// `depth` is the number of CONTAINERS already entered, so the outermost comparison is 0 and the boundary reads as `>
/// limit`: a value nested 10001 levels compares at depths 0..=10000 and succeeds, one nested 10002 levels reaches 10001
/// and raises. Incrementing a counter per level is the whole cost — there is no per-comparison check on the scalar
/// paths.
///
/// `pub(crate)` for [`super::keyed`]'s unboxed key compare, which must enter the virtual box level `One` implies at the
/// same depth the boxed path does.
pub fn compare(left: &Value, right: &Value, depth: usize, law: NanLaw) -> Result<Ordering, TooDeep> {
    if depth > depth::limit(Guarded::Comparison) {
        return Err(TooDeep);
    }
    let left = left.untagged();
    let right = right.untagged();
    match rank(left).cmp(&rank(right)) {
        Ordering::Equal => within_rank(left, right, depth, law),
        other => Ok(other),
    }
}

/// The type rank in jqf's total order, as `contracts.md` spells it:
/// `Null < False < True < Number < String < Bytes < LocalDate < LocalTime < LocalDateTime < OffsetDateTime < Array <
/// Object`.
///
/// Every non-JSON kind has its OWN rank. That is what makes "values from different temporal categories are never equal"
/// true by construction, and it is why the five of them cannot collapse into one another the way a shared rank with no
/// sub-order let them.
fn rank(value: &Value) -> u8 {
    match value {
        Value::Null => 0,
        Value::Bool(false) => 1,
        Value::Bool(true) => 2,
        Value::Number(_) => 3,
        Value::String(_) => 4,
        Value::Bytes(_) => 5,
        Value::LocalDate(_) => 6,
        Value::LocalTime(_) => 7,
        Value::LocalDateTime(_) => 8,
        Value::OffsetDateTime(_) => 9,
        Value::Array(_) => 10,
        // A tagged value is unwrapped by the caller before `rank` runs, so the `Tagged` half of this arm is
        // unreachable; it shares the object rank rather than opening a twelfth rank nothing can occupy.
        Value::Object(_) | Value::Tagged { .. } => 11,
    }
}

/// Orders two values already known to share a type rank.
fn within_rank(left: &Value, right: &Value, depth: usize, law: NanLaw) -> Result<Ordering, TooDeep> {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => Ok(compare_numbers(a, b, law)),
        (Value::String(a), Value::String(b)) => Ok(a.as_str().cmp(b.as_str())),
        (Value::Bytes(a), Value::Bytes(b)) => Ok(a.as_ref().cmp(b.as_ref())),
        // The three local kinds order by their fields in the documented order, which is exactly `jqf-data`'s derived
        // `Ord`: a date by (year, month, day), a time by (hour, minute, second, fraction), a date-time by (date, time).
        // The fraction is normalized digits with trailing zeroes removed, so its byte order IS its decimal order.
        (Value::LocalDate(a), Value::LocalDate(b)) => Ok(a.cmp(b)),
        (Value::LocalTime(a), Value::LocalTime(b)) => Ok(a.cmp(b)),
        (Value::LocalDateTime(a), Value::LocalDateTime(b)) => Ok(a.cmp(b)),
        (Value::OffsetDateTime(a), Value::OffsetDateTime(b)) => Ok(compare_instants(a, b)),
        (Value::Array(a), Value::Array(b)) => compare_arrays(a, b, depth, law),
        (Value::Object(a), Value::Object(b)) => compare_objects(a, b, depth, law),
        // Null and the two booleans carry no sub-order within their rank, and every other same-rank pairing is
        // unreachable now that each kind ranks alone.
        _ => Ok(Ordering::Equal),
    }
}

/// Orders two offset date-times by the instant each represents.
///
/// Whole seconds first, then the arbitrary-precision fraction: the fraction is normalized (no trailing zeroes) so
/// comparing its digits as bytes is comparing it as a decimal.
fn compare_instants(left: &OffsetDateTime, right: &OffsetDateTime) -> Ordering {
    instant_seconds(left).cmp(&instant_seconds(right)).then_with(|| {
        left.local
            .time
            .fraction()
            .digits()
            .cmp(right.local.time.fraction().digits())
    })
}

/// The whole seconds from the epoch of the instant an offset date-time denotes.
///
/// The `-00:00` unknown-local marker contributes an effective offset of zero, per `contracts.md`: it is retained
/// structurally and ignored semantically.
/// The assembly is the shared law in `jqf_data::epoch_seconds_from_civil_parts`; delegating keeps the unknown-local
/// rule and the offset arithmetic in exactly one place.
fn instant_seconds(value: &OffsetDateTime) -> i64 {
    let date = value.local.date;
    let time = &value.local.time;
    let offset = match value.offset {
        UtcOffset::KnownSeconds(known) => i64::from(known.seconds()),
        UtcOffset::UnknownLocalOffset => 0,
    };
    // The checked twin keeps a broken field invariant from becoming a silently wrapped sort key. Every producer of
    // these values goes through the range-validating constructors, so `None` is unreachable; the fallback keeps the
    // comparator total if that invariant is ever broken.
    let seconds = jqf_data::try_epoch_seconds_from_civil_parts(
        i64::from(date.year()),
        i64::from(date.month()),
        i64::from(date.day()),
        i64::from(time.hour()),
        i64::from(time.minute()),
        i64::from(time.second()),
        offset,
    );
    debug_assert!(seconds.is_some(), "temporal fields are constructor-validated");
    seconds.unwrap_or_else(|| {
        jqf_data::epoch_seconds_from_civil_parts(
            i64::from(date.year()),
            i64::from(date.month()),
            i64::from(date.day()),
            i64::from(time.hour()),
            i64::from(time.minute()),
            i64::from(time.second()),
            offset,
        )
    })
}

/// Compares two numbers by exact mathematical value. Any binary64 operand routes through a binary64 comparison; two
/// exact operands compare by scaled big integers so `9007199254740993` and `9007199254740992` stay distinct.
///
/// This is THE leaf the two NaN laws part at, and the only one. Both put NaN at the bottom of the number band, so
/// against a non-NaN operand either law answers identically and Rust sort sees a consistent comparator; only the
/// NaN-against-NaN answer distinguishes them (`Total` says `Equal`, `Observable` says `Less`). See the module note.
fn compare_numbers(left: &Number, right: &Number, law: NanLaw) -> Ordering {
    if left.category() == NumberCategory::Float || right.category() == NumberCategory::Float {
        let (left, right) = (to_f64(left), to_f64(right));
        return match (left.is_nan(), right.is_nan()) {
            (true, true) => match law {
                NanLaw::Total => Ordering::Equal,
                NanLaw::Observable => Ordering::Less,
            },
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            // Neither operand is NaN, so `partial_cmp` is total here and the fallback is unreachable rather than a
            // chosen answer.
            (false, false) => left.partial_cmp(&right).unwrap_or(Ordering::Equal),
        };
    }
    if let (Some(a), Some(b)) = (exact_i64(left), exact_i64(right)) {
        return a.cmp(&b);
    }
    // The machine scaled-decimal fast path, mirroring [`super::arith::try_machine_decimal`]: both operands are exact
    // and machine-sized (integers as scale-zero decimals), the scales align with one checked multiplication, and an
    // overflow DECLINES to the exact alignment below, which computes the identical ordering.
    if let (Some((left_coefficient, left_scale)), Some((right_coefficient, right_scale))) = (
        super::arith::machine_decimal(left),
        super::arith::machine_decimal(right),
    ) && let Some(ordering) = compare_machine_decimals(left_coefficient, left_scale, right_coefficient, right_scale)
    {
        return ordering;
    }
    // Both INTEGER category with at least one heap arm: the canonical signed text IS the exact value, so compare sign,
    // digit count, then bytes — no `BigInt` parse per comparison (the `exact_parts` fallback below parsed every pair,
    // which is the dominant cost of a big-integer sort).
    if left.category() == NumberCategory::Integer
        && right.category() == NumberCategory::Integer
        && let Some(ordering) = compare_integer_text(left, right)
    {
        return ordering;
    }
    let (ca, sa) = exact_parts(left);
    let (cb, sb) = exact_parts(right);
    compare_exact(&ca, sa, &cb, sb)
}

/// The exact ordering of two `coefficient * 10^-scale` pairs, without materializing an aligned coefficient.
///
/// Sign first, then the ADJUSTED EXPONENT — digit count minus scale — which fixes the value's order of magnitude
/// (`coeff * 10^-scale` lies in `[10^(d-1-scale), 10^(d-scale))`), so a larger adjusted exponent means a larger
/// magnitude and only a TIE needs the aligned coefficient comparison.
/// The tie's alignment is then bounded by the operand sizes — equal adjusted exponents make the scale gap equal the
/// digit-count gap — and is read by pure indexing ([`BigInt::cmp_aligned_padded`]), so the fallback that must answer
/// `1e-19 < 1` exactly also never builds a 2^63-digit intermediate for a retained literal whose scale is the whole
/// `i64` range.
fn compare_exact(left: &BigInt, left_scale: i64, right: &BigInt, right_scale: i64) -> Ordering {
    let (left_negative, right_negative) = (left.is_negative(), right.is_negative());
    match (left_negative, right_negative) {
        (false, true) => return Ordering::Greater,
        (true, false) => return Ordering::Less,
        (false, false) | (true, true) => {}
    }
    if left.is_zero() {
        return if right.is_zero() {
            Ordering::Equal
        } else if right_negative {
            Ordering::Greater
        } else {
            Ordering::Less
        };
    }
    if right.is_zero() {
        return if left_negative {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    // Both nonzero and equally signed, so a positive result decides the order and a negative one reverses it.
    let left_exponent = i128::try_from(left.digit_count()).unwrap_or(0) - 1 - i128::from(left_scale);
    let right_exponent = i128::try_from(right.digit_count()).unwrap_or(0) - 1 - i128::from(right_scale);
    let by_exponent = left_exponent.cmp(&right_exponent);
    if by_exponent != Ordering::Equal {
        return if left_negative {
            by_exponent.reverse()
        } else {
            by_exponent
        };
    }
    let target = left_scale.max(right_scale);
    let left_pad = usize::try_from(target - left_scale).unwrap_or(0);
    let right_pad = usize::try_from(target - right_scale).unwrap_or(0);
    // [`BigInt::cmp_aligned_padded`] refuses only past its magnitude ceiling or on pad overflow; with equal adjusted
    // exponents the aligned length is the larger digit count plus the scale gap, and an operand whose digits do not fit
    // memory cannot reach either refusal — the equal answer is unreachable, not a swallowed mismatch.
    let Some(by_digits) = left.cmp_aligned_padded(left_pad, right, right_pad) else {
        debug_assert!(
            false,
            "aligned compare refuses only past ceilings unreachable at equal adjusted exponents"
        );
        return Ordering::Equal;
    };
    if left_negative { by_digits.reverse() } else { by_digits }
}

/// The exact ordering of two INTEGER-category numbers from their canonical signed text: sign, then digit count, then
/// bytes. A zero spelling is not negative (`-0 == 0`), matching the value law `exact_i64` answers for the retained
/// `-0`.
fn compare_integer_text(left: &Number, right: &Number) -> Option<Ordering> {
    let left = left.as_integer()?;
    let right = right.as_integer()?;
    Some(compare_signed_digits(left.as_str(), right.as_str()))
}

/// The exact ordering of two canonical signed digit runs.
fn compare_signed_digits(left: &str, right: &str) -> Ordering {
    let (left_negative, left_digits) = match left.strip_prefix('-') {
        Some(digits) => (digits != "0", digits),
        None => (false, left),
    };
    let (right_negative, right_digits) = match right.strip_prefix('-') {
        Some(digits) => (digits != "0", digits),
        None => (false, right),
    };
    match (left_negative, right_negative) {
        (false, false) => compare_unsigned_digits(left_digits, right_digits),
        (true, true) => compare_unsigned_digits(right_digits, left_digits),
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
    }
}

/// Digit count then byte order over canonical unsigned digit runs: the value comparison for equal-length runs is the
/// lexicographic one.
fn compare_unsigned_digits(left: &str, right: &str) -> Ordering {
    match left.len().cmp(&right.len()) {
        Ordering::Equal => left.cmp(right),
        length => length,
    }
}

/// One `i64` comparison of two machine decimals aligned to the LARGER scale, or `None` when the alignment overflows the
/// machine form and the exact path must compare. The alignment is [`compare_numbers`]'s own: the smaller-scale
/// coefficient is multiplied by `10^gap`, so equal values compare equal (`0.5 == 0.50`) and the ordering is the exact
/// ordering.
fn compare_machine_decimals(
    left_coefficient: i64,
    left_scale: i64,
    right_coefficient: i64,
    right_scale: i64,
) -> Option<Ordering> {
    if left_scale == right_scale {
        return Some(left_coefficient.cmp(&right_coefficient));
    }
    // The scale gap can overflow `i64` (opposite-extreme spellings like `1e9223372036854775807 <
    // 1e-9223372036854775807`), so it is computed checked and an overflowing gap declines to the exact path, whose
    // adjusted-exponent comparison answers it without an aligned intermediate.
    if left_scale < right_scale {
        let gap = right_scale.checked_sub(left_scale)?;
        let aligned = left_coefficient.checked_mul(*super::arith::POW10.get(usize::try_from(gap).ok()?)?)?;
        return Some(aligned.cmp(&right_coefficient));
    }
    let gap = left_scale.checked_sub(right_scale)?;
    let aligned = right_coefficient.checked_mul(*super::arith::POW10.get(usize::try_from(gap).ok()?)?)?;
    Some(left_coefficient.cmp(&aligned))
}

/// Compares two arrays lexicographically, then by length (a proper prefix is less).
///
/// `pub(crate)` for [`super::keyed`]'s `Many`-versus-`Many` arm.
pub fn compare_arrays(
    left: &jqf_data::Array,
    right: &jqf_data::Array,
    depth: usize,
    law: NanLaw,
) -> Result<Ordering, TooDeep> {
    let mut index = 0;
    loop {
        match (left.get(index), right.get(index)) {
            (Some(a), Some(b)) => match compare(a, b, depth + 1, law)? {
                Ordering::Equal => index += 1,
                other => return Ok(other),
            },
            (Some(_), None) => return Ok(Ordering::Greater),
            (None, Some(_)) => return Ok(Ordering::Less),
            (None, None) => return Ok(Ordering::Equal),
        }
    }
}

/// Compares two objects by their sorted key lists, then by values in sorted-key order (the object order).
fn compare_objects(
    left: &jqf_data::Object,
    right: &jqf_data::Object,
    depth: usize,
    law: NanLaw,
) -> Result<Ordering, TooDeep> {
    let left_keys = sorted_key_refs(left);
    let right_keys = sorted_key_refs(right);
    match left_keys.as_slice().cmp(right_keys.as_slice()) {
        Ordering::Equal => {}
        other => return Ok(other),
    }
    for key in &left_keys {
        let a = left.get(key);
        let b = right.get(key);
        match (a, b) {
            (Some(a), Some(b)) => match compare(a, b, depth + 1, law)? {
                Ordering::Equal => {}
                other => return Ok(other),
            },
            // Equal sorted key lists guarantee both lookups resolve.
            _ => return Ok(Ordering::Equal),
        }
    }
    Ok(Ordering::Equal)
}

/// The object's keys in ascending Unicode-scalar (UTF-8 byte) order, as borrowed key strings. Sorting `&str`s (one
/// `Vec` allocation, no per-key heap copy) instead of owned `String`s: the keys outlive the comparison through the
/// borrow of the object, and the sort order is identical.
fn sorted_key_refs(object: &jqf_data::Object) -> Vec<&str> {
    let mut keys = Vec::with_capacity(object.len());
    let mut index = 0;
    while let Some(entry) = object.get_index(index) {
        keys.push(entry.key());
        index += 1;
    }
    keys.sort_unstable();
    keys
}

/// The exact `i64` value of an integer-category operand, or `None` when the operand is not an integer or does not fit
/// one.
///
/// Two operands that both answer `Some` compare by their machine values, which is EXACT (not an approximation) and
/// skips the `BigInt` scaling below. It is deliberately value-based rather than sign-and-magnitude-based, because the
/// heap arm still admits the retained `-0` that `Integer::from_canonical` accepts: a sign-first short-circuit would
/// rank that BELOW a machine `0` instead of Equal to it. Answering from the value closes that hole by construction —
/// `-0` answers `Some(0)`, and `0 == 0`.
fn exact_i64(number: &Number) -> Option<i64> {
    if number.category() != NumberCategory::Integer {
        return None;
    }
    number.to_i64()
}

/// The `(coefficient, scale)` of an exact (non-float) number.
fn exact_parts(number: &Number) -> (BigInt, i64) {
    match number.category() {
        NumberCategory::Integer => (
            number
                .to_integer()
                .and_then(|integer| BigInt::parse(integer.as_str()))
                .unwrap_or_else(BigInt::zero),
            0,
        ),
        NumberCategory::Decimal => number.as_decimal().map_or((BigInt::zero(), 0), |decimal| {
            (
                BigInt::parse(decimal.coefficient().as_str()).unwrap_or_else(BigInt::zero),
                decimal.scale(),
            )
        }),
        NumberCategory::Float => (BigInt::zero(), 0),
    }
}

/// The binary64 value of any number (for float-involving comparisons and the string-repeat count).
pub fn to_f64(number: &Number) -> f64 {
    match number.category() {
        // A machine integer casts directly — one IEEE rounding, the same one the decimal round trip performs —
        // instead of rendering its digits and re-parsing them on every comparison (`arith::contagion_f64`'s machine arm
        // is the same law).
        NumberCategory::Integer => match number.as_machine() {
            #[allow(
                clippy::cast_precision_loss,
                reason = "binary64 conversion is lossy by law; the cast is the same round-to-
                          nearest the decimal round trip it replaces performs"
            )]
            Some(value) => value as f64,
            None => number
                .as_integer()
                .and_then(|integer| integer.as_str().parse().ok())
                .unwrap_or(f64::NAN),
        },
        NumberCategory::Decimal => number.as_decimal().map_or(f64::NAN, jqf_data::Decimal::to_f64),
        NumberCategory::Float => number.as_float().map_or(f64::NAN, jqf_data::Float::get),
    }
}

/// The binary64 value of a BORROWED located number — [`to_f64`]'s law across the document-backed numeric
/// representations, so a located operand never has to be materialized just to be read as a float (the `.[$x]` dynamic
/// index over a located slot is the caller).
pub fn view_to_f64(number: NumberView<'_>) -> f64 {
    match number {
        NumberView::Number(number) => to_f64(number),
        NumberView::Integer(text) => text.parse().unwrap_or(f64::NAN),
        NumberView::Decimal { coefficient, scale } => jqf_data::decimal_parts_to_f64(coefficient, scale),
        NumberView::Float(value) => value.get(),
    }
}

#[cfg(test)]
mod tests {
    use super::{UtcOffset, observable_cmp, semantic_eq, total_cmp};
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cmp::Ordering;
    use jqf_data::{Array, Number, ObjectBuilder, Value};
    fn number(spelling: &str) -> Value {
        Value::Number(Number::try_json_literal(spelling).expect("literal"))
    }

    fn text(value: &str) -> Value {
        Value::try_string(value).expect("fixture string")
    }

    /// The one NaN number the `nan` builtin constructs.
    fn nan() -> Value {
        Value::Number(Number::float(jqf_data::Float::new(f64::NAN)))
    }

    /// The comparator — `total_cmp`, `semantic_eq` and `to_f64` — across the machine/heap arm boundary.
    ///
    /// The exact-`i64` fast path answers whenever BOTH operands fit; otherwise the `BigInt` scaling does. The rows
    /// straddle that split deliberately, so the two routes are shown agreeing on the same value pairs.
    /// The total order, for the tests that assert an ordering on values far shallower than the depth cap: a `TooDeep`
    /// here would be a bug in the guard, not a property of the pair, so it panics rather than folding into a comparison
    /// result.
    fn cmp(left: &Value, right: &Value) -> Ordering {
        total_cmp(left, right).expect("shallow values compare")
    }

    /// See [`cmp`]; the equality spelling.
    fn eq(left: &Value, right: &Value) -> bool {
        semantic_eq(left, right).expect("shallow values compare")
    }

    /// The one rule, at the leaf it is defined on: NaN is less than every number, `-1e308` included, and `1 < nan` is
    /// false. Both laws agree here — which is why sort placement needs no observable special case.
    #[test]
    fn nan_sorts_below_every_number_under_both_laws() {
        let nan = nan();
        let one = number("1");
        let negative = number("-1e308");
        for law in [total_cmp, observable_cmp] {
            assert_eq!(law(&nan, &one).expect("shallow"), Ordering::Less);
            assert_eq!(law(&one, &nan).expect("shallow"), Ordering::Greater);
            assert_eq!(law(&nan, &negative).expect("shallow"), Ordering::Less);
            assert_eq!(law(&negative, &nan).expect("shallow"), Ordering::Greater);
        }
        // Type RANK still outranks the number band: `null < nan` and `nan < "a"` are both true, `true < nan` too.
        assert_eq!(cmp(&Value::Null, &nan), Ordering::Less);
        assert_eq!(cmp(&nan, &text("a")), Ordering::Less);
        assert_eq!(cmp(&Value::Bool(true), &nan), Ordering::Less);
    }

    /// The one leaf the laws part at, and the observable consequences:
    /// `nan == nan` is false and a container carrying a NaN is poisoned, while the internal order stays a consistent
    /// total order the sort can run on.
    #[test]
    fn nan_against_nan_is_equal_internally_and_less_observably() {
        // TWO INDEPENDENT constructions: the observable law answers false for independent NaNs — the
        // allocation-identity short-circuit (which now covers numbers) makes the SAME handle equal to itself, which is
        // the `[nan] as $x | $x == $x` row below, not this one.
        let value = nan();
        let twin = nan();
        assert_eq!(total_cmp(&value, &twin).expect("shallow"), Ordering::Equal);
        assert_eq!(observable_cmp(&value, &twin).expect("shallow"), Ordering::Less);
        assert!(!eq(&value, &twin));

        let boxed = || {
            let mut array = Array::try_new().expect("fixture array");
            array.try_push(nan()).expect("fixture array element");
            Value::Array(array)
        };
        // Structural poisoning: the recursion stops at the NaN leaf with `Less` rather than continuing past it, so
        // `[nan] == [nan]` is false.
        assert!(!eq(&boxed(), &boxed()));
        assert_eq!(observable_cmp(&boxed(), &boxed()).expect("shallow"), Ordering::Less);
        assert_eq!(total_cmp(&boxed(), &boxed()).expect("shallow"), Ordering::Equal);
    }

    /// The allocation-identity short-circuit survives the law, and it is observable: `[nan] as $x | $x == $x` is TRUE
    /// where `[nan] == [nan]` is FALSE over two separate allocations.
    #[test]
    fn allocation_identity_still_short_circuits_over_a_nan() {
        let mut array = Array::try_new().expect("fixture array");
        array.try_push(nan()).expect("fixture array element");
        let value = Value::Array(array);
        let same = value.clone();
        assert!(eq(&value, &same));
        // The false side of the contrast: two INDEPENDENT NaN-carrying allocations are structurally poisoned (the
        // sibling test's law).
        let mut other = Array::try_new().expect("fixture array");
        other.try_push(nan()).expect("fixture array element");
        let distinct = Value::Array(other);
        assert!(!eq(&value, &distinct));
    }

    #[test]
    fn number_comparison_is_blind_to_the_integer_storage_arm() {
        let cases: &[(&str, &str, Ordering)] = &[
            // Both machine.
            ("0", "0", Ordering::Equal),
            ("1", "2", Ordering::Less),
            ("-1", "1", Ordering::Less),
            ("-10", "-9", Ordering::Less),
            // Byte order and numeric order DISAGREE here, which is the point:
            // `"10" < "9"` as text, `10 > 9` as value.
            ("10", "9", Ordering::Greater),
            ("100", "99", Ordering::Greater),
            ("9007199254740993", "9007199254740992", Ordering::Greater),
            ("9223372036854775807", "9223372036854775806", Ordering::Greater),
            ("-9223372036854775808", "-9223372036854775807", Ordering::Less),
            ("-9223372036854775808", "9223372036854775807", Ordering::Less),
            // Machine against heap, both orders: one step past `i64`.
            ("9223372036854775807", "9223372036854775808", Ordering::Less),
            ("9223372036854775808", "9223372036854775807", Ordering::Greater),
            ("-9223372036854775809", "-9223372036854775808", Ordering::Less),
            ("1", "12345678901234567890123456789", Ordering::Less),
            ("-12345678901234567890123456789", "1", Ordering::Less),
            // Both heap.
            ("9223372036854775808", "9223372036854775809", Ordering::Less),
            (
                "12345678901234567890123456789",
                "12345678901234567890123456789",
                Ordering::Equal,
            ),
            // Machine against decimal and against a float-valued expression:
            // neither route is the i64 one, and both must still be exact.
            ("1", "1.0", Ordering::Equal),
            ("1", "1.5", Ordering::Less),
            ("2", "1.5", Ordering::Greater),
            ("1000", "1e3", Ordering::Equal),
            ("9007199254740993", "9007199254740992.0", Ordering::Greater),
        ];
        for (left, right, expected) in cases {
            let (a, b) = (number(left), number(right));
            assert_eq!(cmp(&a, &b), *expected, "{left} vs {right}");
            assert_eq!(cmp(&b, &a), expected.reverse(), "{right} vs {left} (reversed)");
            assert_eq!(eq(&a, &b), *expected == Ordering::Equal, "eq {left} vs {right}");
        }
    }

    /// The one pair with the SAME value and DIFFERENT bytes: a retained `-0` integer against the machine zero.
    ///
    /// The machine arm refuses `-0` by construction, so this pair is the only cross-arm equality in the whole
    /// representation — and it is exactly the one a sign-first short-circuit would have got wrong, ranking the
    /// negative-signed operand BELOW zero. Comparing by VALUE closes the hole.
    #[test]
    fn a_retained_negative_zero_compares_equal_to_the_machine_zero() {
        let retained = Value::Number(Number::integer(
            jqf_data::Integer::from_canonical(String::from("-0")).expect("canonical"),
        ));
        let machine = number("0");
        assert_eq!(retained_bytes(&retained), "-0");
        assert_eq!(retained_bytes(&machine), "0");
        assert_eq!(cmp(&retained, &machine), Ordering::Equal);
        assert_eq!(cmp(&machine, &retained), Ordering::Equal);
        assert!(eq(&retained, &machine));
        // …and it ranks against nonzero neighbours the way zero does.
        assert_eq!(cmp(&retained, &number("1")), Ordering::Less);
        assert_eq!(cmp(&retained, &number("-1")), Ordering::Greater);
        assert_eq!(cmp(&retained, &number("0.0")), Ordering::Equal);
        // `to_f64` is the float-involving route; the retained sign must not make it order differently either.
        assert_eq!(
            cmp(&retained, &Value::Number(Number::float(jqf_data::Float::new(0.0)))),
            Ordering::Equal
        );
    }

    fn retained_bytes(value: &Value) -> String {
        let Value::Number(number) = value else {
            panic!("expected a number");
        };
        String::from(number.to_integer().expect("integer").as_str())
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

    #[test]
    fn cross_category_numeric_equality_and_exactness() {
        assert!(eq(&number("1"), &number("1.0")));
        assert!(!eq(&number("9007199254740993"), &number("9007199254740992")));
        // -0 and 0 are equal, and -0 is not less than 0.
        assert!(eq(&number("-0"), &number("0")));
        assert_eq!(cmp(&number("-0"), &number("0")), Ordering::Equal);
    }

    #[test]
    fn type_rank_orders_across_kinds() {
        assert_eq!(cmp(&Value::Null, &Value::Bool(false)), Ordering::Less);
        assert_eq!(cmp(&Value::Bool(false), &Value::Bool(true)), Ordering::Less);
        assert_eq!(cmp(&number("5"), &text("a")), Ordering::Less);
        assert_eq!(cmp(&text("a"), &array(vec![])), Ordering::Less);
        assert_eq!(cmp(&array(vec![]), &object(vec![])), Ordering::Less);
    }

    #[test]
    fn arrays_are_lexicographic_then_by_length() {
        assert_eq!(
            cmp(
                &array(vec![number("1"), number("2")]),
                &array(vec![number("1"), number("3")])
            ),
            Ordering::Less
        );
        assert_eq!(
            cmp(&array(vec![number("1")]), &array(vec![number("1"), number("2")])),
            Ordering::Less
        );
    }

    #[test]
    fn objects_compare_by_sorted_keys_then_values() {
        assert!(eq(
            &object(vec![("a", number("1")), ("b", number("2"))]),
            &object(vec![("b", number("2")), ("a", number("1"))])
        ));
        assert_eq!(
            cmp(&object(vec![("a", number("1"))]), &object(vec![("a", number("2"))])),
            Ordering::Less
        );
    }

    #[test]
    fn strings_are_codepoint_ordered() {
        assert_eq!(cmp(&text("a"), &text("b")), Ordering::Less);
    }

    fn local_date(year: u16, month: u8, day: u8) -> Value {
        Value::LocalDate(jqf_data::LocalDate::new(year, month, day).expect("date"))
    }

    fn local_time(hour: u8, minute: u8, second: u8, fraction: &str) -> jqf_data::LocalTime {
        jqf_data::LocalTime::new(
            hour,
            minute,
            second,
            jqf_data::FractionalSecond::parse(fraction).expect("fraction"),
        )
        .expect("time")
    }

    fn local_date_time(
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
        fraction: &str,
    ) -> jqf_data::LocalDateTime {
        jqf_data::LocalDateTime {
            date: jqf_data::LocalDate::new(year, month, day).expect("date"),
            time: local_time(hour, minute, second, fraction),
        }
    }

    /// One offset date-time from its parts; `offset` is in seconds, `None` is the `-00:00` unknown-local marker.
    #[allow(clippy::too_many_arguments, reason = "one fixture per RFC 3339 field")]
    fn offset_date_time(
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
        fraction: &str,
        offset: Option<i32>,
    ) -> Value {
        Value::OffsetDateTime(jqf_data::OffsetDateTime {
            local: local_date_time(year, month, day, hour, minute, second, fraction),
            offset: match offset {
                Some(seconds) => UtcOffset::KnownSeconds(jqf_data::KnownUtcOffset::new(seconds).expect("offset")),
                None => UtcOffset::UnknownLocalOffset,
            },
        })
    }

    fn bytes(payload: &[u8]) -> Value {
        Value::try_bytes(payload).expect("fixture bytes")
    }

    /// The whole beyond-compat half of the rank ladder, in the order `contracts.md` spells it. Every neighbouring pair
    /// is strictly ordered, which is the property the old shared rank destroyed: five kinds at one rank with no
    /// sub-order made January equal June.
    #[test]
    fn the_beyond_compat_kinds_each_hold_their_own_rank() {
        let ladder = [
            Value::Null,
            Value::Bool(false),
            Value::Bool(true),
            number("1"),
            text("z"),
            bytes(b"\x00"),
            local_date(2026, 6, 15),
            Value::LocalTime(local_time(12, 34, 56, "")),
            Value::LocalDateTime(local_date_time(2026, 6, 15, 12, 34, 56, "")),
            offset_date_time(2026, 6, 15, 12, 34, 56, "", Some(0)),
            array(vec![]),
            object(vec![]),
        ];
        for (index, left) in ladder.iter().enumerate() {
            for right in &ladder[index + 1..] {
                assert_eq!(cmp(left, right), Ordering::Less);
                assert_eq!(cmp(right, left), Ordering::Greater);
                assert!(!eq(left, right));
            }
            assert_eq!(cmp(left, left), Ordering::Equal);
        }
    }

    #[test]
    fn local_kinds_order_by_their_documented_fields() {
        assert_eq!(cmp(&local_date(2026, 1, 1), &local_date(2026, 6, 15)), Ordering::Less);
        assert_eq!(cmp(&local_date(2026, 6, 15), &local_date(2027, 1, 1)), Ordering::Less);
        assert_eq!(
            cmp(
                &Value::LocalTime(local_time(0, 0, 0, "")),
                &Value::LocalTime(local_time(12, 34, 56, ""))
            ),
            Ordering::Less
        );
        // The fraction is normalized decimal digits: `.5` is greater than `.25` even though "25" sorts before "5" only
        // under decimal reading, and `.5` equals `.50` because the trailing zero never survives parsing.
        assert_eq!(
            cmp(
                &Value::LocalTime(local_time(0, 0, 0, "25")),
                &Value::LocalTime(local_time(0, 0, 0, "5"))
            ),
            Ordering::Less
        );
        assert!(eq(
            &Value::LocalTime(local_time(0, 0, 0, "5")),
            &Value::LocalTime(local_time(0, 0, 0, "50"))
        ));
        // The syntactic leap-second label stays distinct in a LOCAL time.
        assert_eq!(
            cmp(
                &Value::LocalTime(local_time(23, 59, 59, "")),
                &Value::LocalTime(local_time(23, 59, 60, ""))
            ),
            Ordering::Less
        );
        assert_eq!(
            cmp(
                &Value::LocalDateTime(local_date_time(2026, 1, 1, 23, 0, 0, "")),
                &Value::LocalDateTime(local_date_time(2026, 1, 2, 0, 0, 0, ""))
            ),
            Ordering::Less
        );
    }

    /// An offset date-time is its INSTANT: two spellings of one instant are equal, and a later wall clock in a
    /// far-enough-east offset is EARLIER.
    #[test]
    fn offset_date_times_compare_by_instant() {
        let utc_noon = offset_date_time(2026, 6, 15, 12, 0, 0, "", Some(0));
        let plus_two = offset_date_time(2026, 6, 15, 14, 0, 0, "", Some(2 * 3600));
        assert!(eq(&utc_noon, &plus_two));
        assert_eq!(cmp(&utc_noon, &plus_two), Ordering::Equal);
        // A minute-and-second offset scales correctly: +04:02 is 14520 seconds, never 14402 — the comparison must not
        // inherit a codec's arithmetic.
        let plus_four_oh_two = offset_date_time(2026, 6, 15, 16, 2, 0, "", Some(4 * 3600 + 2 * 60));
        assert!(eq(&utc_noon, &plus_four_oh_two));
        // Later wall clock, further east: the instant is EARLIER.
        let east = offset_date_time(2026, 6, 15, 13, 0, 0, "", Some(4 * 3600));
        assert_eq!(cmp(&east, &utc_noon), Ordering::Less);
        // The `-00:00` unknown-local marker has effective offset zero.
        assert!(eq(&utc_noon, &offset_date_time(2026, 6, 15, 12, 0, 0, "", None)));
        // Across the year boundary and across the date line, both directions.
        assert_eq!(
            cmp(
                &offset_date_time(2026, 1, 1, 0, 0, 0, "", Some(0)),
                &offset_date_time(2026, 6, 15, 12, 34, 56, "", Some(0))
            ),
            Ordering::Less
        );
        assert_eq!(
            cmp(
                &offset_date_time(2025, 12, 31, 23, 0, 0, "", Some(0)),
                &offset_date_time(2026, 1, 1, 1, 0, 0, "", Some(2 * 3600))
            ),
            Ordering::Equal
        );
        // Sub-second precision breaks an otherwise equal instant.
        assert_eq!(
            cmp(
                &offset_date_time(2026, 6, 15, 12, 0, 0, "25", Some(0)),
                &offset_date_time(2026, 6, 15, 12, 0, 0, "5", Some(0))
            ),
            Ordering::Less
        );
    }

    /// A leap year, a century, and a four-century year: the civil-days arithmetic the instant rests on, checked where
    /// calendars are hardest.
    #[test]
    fn instant_arithmetic_handles_the_gregorian_exceptions() {
        // 2000 is a leap year (divisible by 400), 1900 is not (by 100).
        assert_eq!(
            cmp(
                &offset_date_time(2000, 2, 29, 0, 0, 0, "", Some(0)),
                &offset_date_time(2000, 3, 1, 0, 0, 0, "", Some(0))
            ),
            Ordering::Less
        );
        assert_eq!(
            cmp(
                &offset_date_time(1900, 2, 28, 0, 0, 0, "", Some(0)),
                &offset_date_time(1900, 3, 1, 0, 0, 0, "", Some(0))
            ),
            Ordering::Less
        );
        // One day apart across an offset that shifts the date, both ways.
        assert_eq!(
            cmp(
                &offset_date_time(2026, 3, 1, 1, 0, 0, "", Some(2 * 3600)),
                &offset_date_time(2026, 2, 28, 23, 30, 0, "", Some(0))
            ),
            Ordering::Less
        );
        // The extremes the validated storage admits still order.
        assert_eq!(
            cmp(
                &offset_date_time(0, 1, 1, 0, 0, 0, "", Some(0)),
                &offset_date_time(9999, 12, 31, 23, 59, 59, "", Some(0))
            ),
            Ordering::Less
        );
    }

    #[test]
    fn bytes_order_lexicographically_by_unsigned_byte() {
        assert_eq!(cmp(&bytes(b"\x01\x02"), &bytes(b"\x03\x04")), Ordering::Less);
        assert_eq!(cmp(&bytes(b"\x01"), &bytes(b"\x01\x00")), Ordering::Less);
        assert_eq!(cmp(&bytes(b"\x7f"), &bytes(b"\x80")), Ordering::Less);
        assert!(eq(&bytes(b"\x01\x02"), &bytes(b"\x01\x02")));
    }

    /// The exact-path fallback answers a scale gap past the machine fast path's 18-digit ceiling by exponent, exactly
    /// and without materializing an aligned coefficient: `1e-19 < 1` and `1e-20 < 2` are true.
    #[test]
    fn exact_path_orders_a_scale_gap_past_the_machine_ceiling() {
        assert_eq!(cmp(&number("1e-19"), &number("1")), Ordering::Less);
        assert_eq!(cmp(&number("1e-20"), &number("2")), Ordering::Less);
        assert_eq!(cmp(&number("1e-20"), &number("0.5")), Ordering::Less);
        assert_eq!(cmp(&number("1e19"), &number("1")), Ordering::Greater);
        // The sign survives: a negative machine operand against a negative tiny fraction is MORE negative, never zero.
        assert_eq!(cmp(&number("-5"), &number("-1e-20")), Ordering::Less);
        assert_eq!(cmp(&number("-1e-20"), &number("0")), Ordering::Less);
    }

    /// A retained literal whose scale is the whole `i64` range compares by adjusted exponent alone — the value the
    /// old alignment answered by building a 2^63-digit intermediate (an allocation panic, not an answer).
    #[test]
    fn exact_path_orders_an_i64_scale_literal_without_building_it() {
        let tiny = number("1e-9223372036854775807");
        assert_eq!(cmp(&tiny, &number("1")), Ordering::Less);
        assert_eq!(cmp(&tiny, &number("0.5")), Ordering::Less);
        assert_eq!(cmp(&number("1"), &tiny), Ordering::Greater);
        assert_eq!(cmp(&tiny, &number("0")), Ordering::Greater);
        assert_eq!(cmp(&number("0"), &tiny), Ordering::Less);
        assert_eq!(cmp(&tiny, &tiny), Ordering::Equal);
        assert_eq!(cmp(&tiny, &number("1e-9223372036854775806")), Ordering::Less);
        let huge = number("1e9223372036854775807");
        assert_eq!(cmp(&huge, &number("1")), Ordering::Greater);
        assert_eq!(cmp(&huge, &number("-1e9223372036854775807")), Ordering::Greater);
    }

    /// Equal adjusted exponents align by PURE INDEXING: a 21-digit integer and `1e20` (scale gap 20, past the machine
    /// ceiling) compare equal without shifting either coefficient.
    #[test]
    fn equal_adjusted_exponents_align_without_shifting() {
        assert!(eq(&number("100000000000000000000"), &number("1e20")));
        assert_eq!(cmp(&number("100000000000000000000"), &number("1e20")), Ordering::Equal);
        assert_eq!(cmp(&number("99999999999999999999"), &number("1e20")), Ordering::Less);
        assert_eq!(
            cmp(&number("100000000000000000001"), &number("1e20")),
            Ordering::Greater
        );
    }

    /// Opposite-extreme scales: the machine comparison's alignment gap (`right_scale - left_scale`) overflows `i64` on
    /// this pairing, so the fast path must DECLINE to the exact adjusted-exponent comparison — which answers without
    /// any aligned intermediate — instead of wrapping or panicking a debug build on the subtraction.
    #[test]
    fn opposite_extreme_scale_decimals_compare_exactly() {
        let tiny = number("1e-9223372036854775807");
        let huge = number("1e9223372036854775807");
        assert_eq!(cmp(&tiny, &huge), Ordering::Less);
        assert_eq!(cmp(&huge, &tiny), Ordering::Greater);
        assert!(!eq(&tiny, &huge));
    }
}
