//! The exact-number engine: jqf's arithmetic value law over jqf-data numbers.
//!
//! One job: compute `+ - * / %` over two [`jqf_data::Number`] operands under the pinned jqf number law
//! (builtin-library.md), returning a canonical owned [`Number`]. Exact `Integer`/`Decimal` operands use exact rational
//! intermediates (this is the first observable intentional difference from the double arithmetic): `+ - *` stay
//! ring-closed and never range-error, `/` yields an `Integer` for an integral quotient, a canonical `Decimal` when the
//! reduced denominator is `2^a·5^b`, else the nearest binary64; `%` truncates both operands to exact integers first
//! (sign follows the dividend). Any `Float` operand triggers full binary64 contagion (both operands converted, one IEEE
//! op, `Float` result), and a non-finite result is a VALUE like any other — arithmetic propagates NaN and infinities,
//! and the render path spells them the reference way (NaN as `null`, infinities clamped to the widest binary64).
//!
//! Negative space: it never renders (the codec owns number bytes, including the computed-`Float` shortest-round-trip
//! form), never touches non-number value kinds (the type-error family and the string/array/object operator overloads
//! live in [`crate::semantics::binary`]), drives no cardinality, and holds no state.
//! Comparisons never reach here — they are value law, not arithmetic ([`crate::semantics::order`]).

use alloc::string::{String, ToString};

use jqf_data::{Decimal, Integer, Number, NumberCategory};

use jqf_data::BigInt;

/// The five arithmetic operators this engine computes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArithOp {
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
}

/// A numeric failure inside the exact-number engine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NumError {
    /// `/` by a zero divisor (the distinct "divided" divide-by-zero class).
    DivideByZero,
    /// `%` whose divisor truncates to zero (the distinct "divided (remainder)" class; the truncation happens before
    /// this check).
    RemainderByZero,
    /// A result left the representable range on a DEFENSIVE exact-path arm (a scale gap past `usize`, a canonical
    /// decimal that fails to normalize, a NaN `%` operand past its own pre-check). The binary64 path never raises it: a
    /// non-finite float result is a value like any other.
    NumericRange,
    /// Allocating canonical result storage failed.
    Allocation,
}

/// One numeric operand in a computation-friendly form: an exact big integer, an exact `coefficient * 10^-scale` decimal
/// (coefficient may carry trailing zeros), or a binary64 value.
#[derive(Clone, Debug)]
enum Num {
    Int(BigInt),
    Dec { coeff: BigInt, scale: i64 },
    Float(f64),
}

impl Num {
    /// Extracts a computation operand from an owned [`Number`].
    fn from_number(number: &Number) -> Option<Self> {
        match number.category() {
            NumberCategory::Integer => Some(Self::Int(BigInt::parse(number.to_integer()?.as_str())?)),
            NumberCategory::Decimal => {
                let decimal = number.as_decimal()?;
                Some(Self::Dec {
                    coeff: BigInt::parse(decimal.coefficient().as_str())?,
                    scale: decimal.scale(),
                })
            }
            NumberCategory::Float => Some(Self::Float(number.as_float()?.get())),
        }
    }

    /// The `(coefficient, scale)` view of an exact operand (an integer is scale zero); panics only if called on a
    /// `Float`, which the exact paths exclude.
    fn as_decimal(&self) -> (BigInt, i64) {
        match self {
            Self::Int(value) => (value.clone(), 0),
            Self::Dec { coeff, scale } => (coeff.clone(), *scale),
            Self::Float(_) => unreachable!("exact decimal view over a float operand"),
        }
    }

    /// The nearest binary64 value of this operand (for float contagion).
    fn to_f64(&self) -> f64 {
        match self {
            Self::Int(value) => value.to_decimal_string().parse().unwrap_or(f64::NAN),
            Self::Dec { coeff, scale } => {
                // The exact path only reaches here for coefficients past the machine arm, so the BigInt keeps its
                // decimal `String`; the conversion itself is [`jqf_data::decimal_parts_to_f64`]'s stack-buffer law (and
                // its correct `i64::MIN` exponent).
                jqf_data::decimal_parts_to_f64(&coeff.to_decimal_string(), *scale)
            }
            Self::Float(value) => *value,
        }
    }

    /// Truncates this operand toward zero to an exact integer (the `%` operand rule). A float truncates toward zero and
    /// SATURATES at the i64 range — the pinned law, because the reference double-to-int64 conversion is
    /// platform-unstable (`(1e300/3) % 10` = `7`, `(-infinite) % infinite` = `-1`); a NaN float is refused here and
    /// handled by [`remainder`]'s NaN law before this point.
    fn truncate_to_integer(&self) -> Option<BigInt> {
        match self {
            Self::Int(value) => Some(value.clone()),
            Self::Dec { coeff, scale } => truncate_decimal(coeff, *scale),
            Self::Float(value) => {
                if value.is_nan() {
                    None
                } else {
                    // The reference double-to-int64 conversion is UB on an out-of-range float and platform-unstable, so
                    // the Rust saturating-cast answers are pinned (`inf as i64` -> i64::MAX, `-inf as i64` ->
                    // i64::MIN); the compat corpus must never carry a `%`-with-infinity row that would disagree with
                    // this platform.
                    #[allow(clippy::cast_possible_truncation, reason = "the saturating cast is the pinned law")]
                    BigInt::parse(&(trunc_toward_zero(*value) as i64).to_string())
                }
            }
        }
    }
}

/// Computes `left op right` under the jqf number law.
pub fn compute_number(
    op: ArithOp,
    left: &Number,
    right: &Number,
    resources: &jqf_resource::ResourceContext<'_>,
) -> Result<Number, NumError> {
    if let Some(result) = try_machine_integer(op, left, right) {
        return result;
    }
    if let Some(result) = try_machine_decimal(op, left, right) {
        return result;
    }
    if op != ArithOp::Remainder && (is_float(left) || is_float(right)) {
        return contagion(op, left, right, resources);
    }
    let left = Num::from_number(left).ok_or(NumError::Allocation)?;
    let right = Num::from_number(right).ok_or(NumError::Allocation)?;
    apply(op, &left, &right, resources)
}

fn is_float(number: &Number) -> bool {
    number.category() == NumberCategory::Float
}

/// Binary64 contagion with the operands converted straight to `f64`.
///
/// This is [`apply`]'s float arm reached without building the exact operands first: a machine integer converts as
/// `value as f64` (one IEEE rounding, the same one `i64::to_string().parse::<f64>()` performs) instead of paying a
/// `BigInt` digit vector and its decimal `String` on the way. `%` is excluded because it is NOT contagious — it
/// truncates both operands to exact integers first, which is the exact path's own law.
fn contagion(
    op: ArithOp,
    left: &Number,
    right: &Number,
    resources: &jqf_resource::ResourceContext<'_>,
) -> Result<Number, NumError> {
    // One exact-to-binary64 boundary crossing, surfaced by `--diagnostics`:
    // the op is about to leave the exact number law entirely.
    resources.note_precision_boundary();
    let left = contagion_f64(left).ok_or(NumError::Allocation)?;
    let right = contagion_f64(right).ok_or(NumError::Allocation)?;
    if op == ArithOp::Divide {
        if right == 0.0 {
            return Err(NumError::DivideByZero);
        }
        return Ok(float_result(left / right));
    }
    Ok(float_result(binary64(op, left, right)))
}

/// One operand's binary64 value on the contagion path.
///
/// A float is itself and a machine integer is a direct cast; everything else converts through
/// [`jqf_data::Decimal::to_f64`]'s stack-buffer law, which keeps a retained `-0` coefficient NEGATIVE (the
/// decimal-to-double law:
/// `-0.0 | floor` is `-0`) — the old `BigInt` route normalized it away. A big integer still takes [`Num::to_f64`]'s
/// decimal-`String` route.
fn contagion_f64(number: &Number) -> Option<f64> {
    match number.category() {
        NumberCategory::Float => Some(number.as_float()?.get()),
        NumberCategory::Integer => match machine_integer(number) {
            #[allow(
                clippy::cast_precision_loss,
                reason = "binary64 contagion is lossy by law; this is the same round-to-nearest \
                          the decimal round trip it replaces performs"
            )]
            Some(value) => Some(value as f64),
            None => Some(Num::from_number(number)?.to_f64()),
        },
        NumberCategory::Decimal => Some(number.as_decimal()?.to_f64()),
    }
}

/// Machine-integer fast path for `+ - *`, or `None` to take the exact path.
///
/// It is EXACT, not an approximation, and it is deliberately total in the other direction: it declines whenever the
/// answer is not provably the same one the arbitrary-precision path computes. An integer number's stored text is
/// canonical (`-?[0-9]+`, no redundant leading zeros), so `i64::from_str` succeeds exactly when the value fits an
/// `i64`; `checked_*` declines on overflow, so a result outside `i64` falls through with nothing computed; and `/` and
/// `%` never enter, because their exactness, zero-divisor, and truncation laws are the exact path's own.
///
/// What it buys: the exact path allocates five times per operation (a `BigInt` digit vector per operand, one for the
/// sum, its decimal `String`, and the canonical `Integer`). This allocates once.
fn try_machine_integer(op: ArithOp, left: &Number, right: &Number) -> Option<Result<Number, NumError>> {
    let value = machine_result(op, machine_integer(left)?, machine_integer(right)?)?;
    Some(machine_integer_number(value))
}

/// [`try_machine_integer`]'s law written as an UPDATE of an OWNED left operand:
/// the answer is written into `left` instead of being built beside it.
///
/// `false` means only that this path did not answer — the operands, the operators and the decline discipline are
/// [`try_machine_integer`]'s own, through the shared [`machine_result`], so there is still ONE integer fast path under
/// one law. A decline leaves `left` untouched and the caller takes the general path over it, which computes the
/// identical number.
///
/// What it saves over the borrowed form is the accumulator's allocation: in a fold the left operand is the accumulator
/// and dies as soon as the result exists, so its representation is uniquely held and can simply be overwritten
/// ([`Number::set_integer`]). A shared representation still detaches there, so this is a pure allocation saving and
/// never a semantic one.
pub fn try_machine_integer_in_place(op: ArithOp, left: &mut Number, right: &Number) -> bool {
    let Some(result) = machine_integer(left)
        .zip(machine_integer(right))
        .and_then(|(left, right)| machine_result(op, left, right))
    else {
        return false;
    };
    {
        left.set_integer(Integer::from_i64(result));
        true
    }
}

/// The exact `i64` answer for `left op right`, or `None` when this path must decline — because the operator is not
/// one it owns (`/` and `%` keep their exactness, zero-divisor and truncation laws on the exact path) or because the
/// result does not fit an `i64` and must promote rather than wrap.
fn machine_result(op: ArithOp, left: i64, right: i64) -> Option<i64> {
    match op {
        ArithOp::Add => left.checked_add(right),
        ArithOp::Subtract => left.checked_sub(right),
        ArithOp::Multiply => left.checked_mul(right),
        ArithOp::Divide | ArithOp::Remainder => None,
    }
}

/// The number's value as an `i64`, or `None` when it is not an integer or does not fit — in which case the caller
/// must take the arbitrary-precision path.
///
/// This is a REPRESENTATION READ, not a parse:
/// [`Integer::to_i64`] answers from the inline arm's own field, and falls back to the very `i64::from_str` this
/// function used to perform for the heap arm, so the decline discipline above is unchanged in every case.
pub fn machine_integer(number: &Number) -> Option<i64> {
    if number.category() != NumberCategory::Integer {
        return None;
    }
    number.to_i64()
}

/// The number's exact value as an `(i64 coefficient, i64 scale)` pair, or `None` when it is not an exact machine-sized
/// operand — a float (whose law is contagion), a heap-form big integer, or a decimal whose coefficient does not fit
/// an `i64` — in which case the caller takes the exact path.
///
/// An integer is the scale-zero decimal (`value * 10^0`), which is what lets one path own `int + decimal`, `decimal +
/// int` and `decimal + decimal` without a third arm. The order law reads the same pair for its machine comparison fast
/// path.
pub fn machine_decimal(number: &Number) -> Option<(i64, i64)> {
    match number.category() {
        NumberCategory::Integer => Some((number.to_i64()?, 0)),
        NumberCategory::Decimal => {
            let decimal = number.as_decimal()?;
            Some((decimal.coefficient().to_i64()?, decimal.scale()))
        }
        NumberCategory::Float => None,
    }
}

/// `10^n` for `n <= 18`, the largest power that fits an `i64`.
pub const POW10: [i64; 19] = [
    1,
    10,
    100,
    1_000,
    10_000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
    1_000_000_000,
    10_000_000_000,
    100_000_000_000,
    1_000_000_000_000,
    10_000_000_000_000,
    100_000_000_000_000,
    1_000_000_000_000_000,
    10_000_000_000_000_000,
    100_000_000_000_000_000,
    1_000_000_000_000_000_000,
];

/// The exact `(coefficient, scale)` answer for `left op right` over two machine decimals, or `None` when this path must
/// decline — the operator is not one it owns (`/` and `%` keep their exactness, zero-divisor and truncation laws on
/// the exact path), or the exact answer cannot fit the machine form and must promote rather than wrap.
///
/// Addition and subtraction align to the LARGER scale, multiplying the smaller-scale operand's coefficient by `10^gap`
/// exactly as the exact path's `mul_pow10` does; multiplication adds scales with no alignment at all.
fn machine_decimal_result(
    op: ArithOp,
    left_coefficient: i64,
    left_scale: i64,
    right_coefficient: i64,
    right_scale: i64,
) -> Option<(i64, i64)> {
    match op {
        ArithOp::Add | ArithOp::Subtract => match left_scale.cmp(&right_scale) {
            core::cmp::Ordering::Equal => Some((machine_result(op, left_coefficient, right_coefficient)?, left_scale)),
            // The alignment multiplies the SMALLER-scale coefficient only; the operator's operands keep their identity,
            // so `-` cannot flip.
            // The scale gap itself can overflow `i64` (scales reach the full range through pinned spellings like
            // `1e-9223372036854775807`), so it is computed checked and an overflowing gap declines to the exact path
            // like every other does-not-fit answer.
            core::cmp::Ordering::Less => {
                let gap = right_scale.checked_sub(left_scale)?;
                let aligned = left_coefficient.checked_mul(*POW10.get(usize::try_from(gap).ok()?)?)?;
                Some((machine_result(op, aligned, right_coefficient)?, right_scale))
            }
            core::cmp::Ordering::Greater => {
                let gap = left_scale.checked_sub(right_scale)?;
                let aligned = right_coefficient.checked_mul(*POW10.get(usize::try_from(gap).ok()?)?)?;
                Some((machine_result(op, left_coefficient, aligned)?, left_scale))
            }
        },
        ArithOp::Multiply => Some((
            left_coefficient.checked_mul(right_coefficient)?,
            left_scale.checked_add(right_scale)?,
        )),
        ArithOp::Divide | ArithOp::Remainder => None,
    }
}

/// The canonical machine form of an exact `(coefficient, scale)` pair whose arithmetic already fit the machine form,
/// mirroring [`decimal_number`]'s normalization and positional-window law in `i64` — or `None` when the canonical
/// form cannot fit the machine form and must promote to the exact path.
///
/// The mirrored law, in order: a zero magnitude collapses to the canonical integer zero; trailing zeroes are stripped
/// from the coefficient with the scale following; an integral result (scale zero) becomes an `Integer`; a NEGATIVE
/// scale is integral too, and renders positionally as an exact `Integer` exactly when the coefficient times `10^-scale`
/// fits an `i64` — `adjusted <= count + 14` collapses to `exponent <= 15` for any coefficient digit count, so the
/// positional window is `exponent <= 15` here — and stays a canonical `Decimal` outside it (`1E+16`).
enum MachineNumber {
    /// A canonical `Integer` value.
    Integer(i64),
    /// A canonical `Decimal` coefficient at a scale (never zero or negative with an in-window exponent: those forms
    /// canonicalized to `Integer`).
    Decimal(i64, i64),
}

fn machine_decimal_parts(coefficient: i64, scale: i64) -> Option<MachineNumber> {
    if coefficient == 0 {
        return Some(MachineNumber::Integer(0));
    }
    let mut coefficient = coefficient;
    let mut scale = scale;
    while coefficient % 10 == 0 {
        coefficient /= 10;
        scale = scale.checked_sub(1)?;
    }
    if scale == 0 {
        return Some(MachineNumber::Integer(coefficient));
    }
    if scale < 0 {
        let exponent = scale.checked_neg()?;
        if exponent <= 15 {
            let pow = *POW10.get(usize::try_from(exponent).ok()?)?;
            return Some(MachineNumber::Integer(coefficient.checked_mul(pow)?));
        }
    }
    Some(MachineNumber::Decimal(coefficient, scale))
}

/// Builds the canonical owned `Number` for a machine exact pair, or `None` when the pair cannot fit the machine form
/// and the exact path must answer.
fn machine_decimal_number(coefficient: i64, scale: i64) -> Option<Result<Number, NumError>> {
    Some(match machine_decimal_parts(coefficient, scale)? {
        MachineNumber::Integer(value) => machine_integer_number(value),
        MachineNumber::Decimal(coefficient, scale) => {
            let decimal = Decimal::from_parts(Integer::from_i64(coefficient), scale).ok()?;
            Number::try_decimal_unaccounted(decimal).map_err(|_| NumError::Allocation)
        }
    })
}

/// The machine scaled-decimal fast path for `+ - *` over exact operands, parallel to [`try_machine_integer`].
///
/// It answers when both operands are exact and machine-sized (integers included, as scale-zero decimals) and the
/// canonical result fits the machine form; it DECLINES — `None` — in every other case, including a float operand
/// (whose law is contagion, reached next) and any overflow (whose law is the exact path, reached after that). A decline
/// computes nothing, so the caller cannot observe it.
///
/// What it buys: the exact path allocates five times per operation (a `BigInt` digit vector per operand, one for the
/// sum, its decimal `String`, and the canonical `Integer`); this path allocates once (the result `Number`) and renders
/// no intermediate. A fold over homogeneous decimal data — the corpus's `add`/`reduce`/`average` lanes — stays on
/// this path for its whole run instead of allocating per step.
pub fn try_machine_decimal(op: ArithOp, left: &Number, right: &Number) -> Option<Result<Number, NumError>> {
    let (left_coefficient, left_scale) = machine_decimal(left)?;
    let (right_coefficient, right_scale) = machine_decimal(right)?;
    let (coefficient, scale) =
        machine_decimal_result(op, left_coefficient, left_scale, right_coefficient, right_scale)?;
    machine_decimal_number(coefficient, scale)
}

/// [`try_machine_decimal`]'s law written as an UPDATE of an OWNED left operand, mirroring
/// [`try_machine_integer_in_place`]: the answer is written into `left` instead of being built beside it, through
/// [`Number::set_integer`] or [`Number::set_decimal`] depending on the canonical category of the result.
///
/// `false` means only that this path did not answer — the decline discipline is [`try_machine_decimal`]'s own, so the
/// caller takes the general path over the operand left untouched, which computes the identical number.
pub fn try_machine_decimal_in_place(op: ArithOp, left: &mut Number, right: &Number) -> bool {
    let Some((left_coefficient, left_scale)) = machine_decimal(left) else {
        return false;
    };
    let Some((right_coefficient, right_scale)) = machine_decimal(right) else {
        return false;
    };
    let Some((coefficient, scale)) =
        machine_decimal_result(op, left_coefficient, left_scale, right_coefficient, right_scale)
    else {
        return false;
    };
    let Some(parts) = machine_decimal_parts(coefficient, scale) else {
        return false;
    };
    match parts {
        MachineNumber::Integer(value) => {
            left.set_integer(Integer::from_i64(value));
            true
        }
        MachineNumber::Decimal(coefficient, scale) => {
            let Ok(decimal) = Decimal::from_parts(Integer::from_i64(coefficient), scale) else {
                return false;
            };
            left.set_decimal(decimal);
            true
        }
    }
}

/// Builds the canonical `Integer` number for an `i64`.
///
/// The stack-buffer decimal render this function used to perform now lives in [`Integer::from_i64`], where the rendered
/// bytes become the value's own inline storage instead of being copied into a heap `String`.
fn machine_integer_number(value: i64) -> Result<Number, NumError> {
    Number::try_integer_unaccounted(Integer::from_i64(value)).map_err(|_| NumError::Allocation)
}

fn apply(
    op: ArithOp,
    left: &Num,
    right: &Num,
    resources: &jqf_resource::ResourceContext<'_>,
) -> Result<Number, NumError> {
    // No float arms here: [`compute_number`] short-circuits every non-`%` float operand through [`contagion`] before
    // this general path is reached, and `%` is `remainder`'s own law, so the only operands that arrive are the exact
    // `Int`/`Dec` pairings.
    match op {
        ArithOp::Add | ArithOp::Subtract | ArithOp::Multiply => additive_or_multiplicative(op, left, right, resources),
        ArithOp::Divide => divide_exact(left, right, resources),
        ArithOp::Remainder => remainder(left, right, resources),
    }
}

/// One binary64 op with the operands already converted.
fn binary64(op: ArithOp, left: f64, right: f64) -> f64 {
    match op {
        ArithOp::Add => left + right,
        ArithOp::Subtract => left - right,
        ArithOp::Multiply => left * right,
        // Division and remainder never route here.
        ArithOp::Divide | ArithOp::Remainder => f64::NAN,
    }
}

/// Exact `+ - *` over two non-float operands. Integer operands stay `Integer`; any decimal operand yields a canonical
/// `Decimal`.
///
/// A decimal scale gap materializes the operand with the smaller scale at the larger one's scale (`1e-100000000 + 1`
/// shifts the `1` a hundred million places); the shift is bounded by the big-integer digit ceiling (`mul_pow10`
/// declines past it with [`NumError::NumericRange`]), so an output-sized intermediate cannot grow unbounded. The
/// request ledger is NOT consulted on this path — the digit ceiling is the only guard.
fn additive_or_multiplicative(
    op: ArithOp,
    left: &Num,
    right: &Num,
    _resources: &jqf_resource::ResourceContext<'_>,
) -> Result<Number, NumError> {
    if let (Num::Int(a), Num::Int(b)) = (left, right) {
        let value = match op {
            ArithOp::Add => a.add(b),
            ArithOp::Subtract => a.sub(b),
            ArithOp::Multiply => a.mul(b),
            ArithOp::Divide | ArithOp::Remainder => unreachable!("routed elsewhere"),
        };
        return integer_number(&value);
    }
    let (ca, sa) = left.as_decimal();
    let (cb, sb) = right.as_decimal();
    let (coeff, scale) = match op {
        ArithOp::Add | ArithOp::Subtract => {
            let scale = sa.max(sb);
            // Each shift is bounded by the big-integer digit ceiling (`mul_pow10` declines past it); the shifted
            // magnitudes stay alive beside each other until the sum replaces them.
            let gap_a = scale_gap(scale, sa)?;
            let ca = ca.mul_pow10(gap_a).ok_or(NumError::NumericRange)?;
            let gap_b = scale_gap(scale, sb)?;
            let cb = cb.mul_pow10(gap_b).ok_or(NumError::NumericRange)?;
            let coeff = if op == ArithOp::Add { ca.add(&cb) } else { ca.sub(&cb) };
            (coeff, scale)
        }
        ArithOp::Multiply => {
            let scale = sa.checked_add(sb).ok_or(NumError::NumericRange)?;
            (ca.mul(&cb), scale)
        }
        ArithOp::Divide | ArithOp::Remainder => unreachable!("routed elsewhere"),
    };
    decimal_number(coeff, scale)
}

/// Exact division: an integral quotient is an `Integer`, a quotient with a `2^a·5^b` denominator is a canonical
/// terminating `Decimal`, and a quotient with no finite decimal expansion rounds into binary64 exactly as the contagion
/// path does — `1 / 3` is `0.3333333333333333`.
///
/// The rounding arm is the only one: the refusal it replaced had no escape hatch — `1/3.0`, `1.0/3` and
/// `(1|tonumber)/3` all raised too, because a JSON literal never decodes to `Float` — so a user could not ask for the
/// rounded quotient at all, and whether a program ran at all depended on the VALUES in the data.
fn divide_exact(left: &Num, right: &Num, resources: &jqf_resource::ResourceContext<'_>) -> Result<Number, NumError> {
    let (ca, sa) = left.as_decimal();
    let (cb, sb) = right.as_decimal();
    if cb.is_zero() {
        return Err(NumError::DivideByZero);
    }
    // value = (ca / cb) * 10^(sb - sa). Fold the power of ten into a rational p / q with q positive. The folded
    // magnitude is bounded by the big-integer digit ceiling (`mul_pow10` declines past it with
    // [`NumError::NumericRange`]) — `1e100000000 / 2` is a genuine 10^8-digit answer, and a shift past the ceiling
    // refuses rather than materialize it; the ledger is not consulted here. The shifted magnitude stays alive beside
    // `q` for the reduction below.
    let exponent = i128::from(sb) - i128::from(sa);
    let (mut p, mut q) = if exponent >= 0 {
        let power = usize_pow(exponent)?;
        let shifted = ca.mul_pow10(power).ok_or(NumError::NumericRange)?;
        (shifted, cb)
    } else {
        let power = usize_pow(-exponent)?;
        let shifted = cb.mul_pow10(power).ok_or(NumError::NumericRange)?;
        (ca, shifted)
    };
    // Move q's sign onto p so q is a positive magnitude.
    if q.is_negative() {
        p = p.negate();
        q = q.abs();
    }
    let negative = p.is_negative();
    let mut p_abs = p.abs();
    // Reduce to lowest terms.
    let gcd = p_abs.gcd(&q);
    if !gcd.is_zero() && gcd != BigInt::from_u64(1) {
        p_abs = p_abs.div_rem(&gcd).ok_or(NumError::Allocation)?.0;
        q = q.div_rem(&gcd).ok_or(NumError::Allocation)?.0;
    }
    let signed_p = if negative { p_abs.negate() } else { p_abs.clone() };
    if q == BigInt::from_u64(1) {
        return integer_number(&signed_p);
    }
    // Terminating iff the reduced denominator is 2^i · 5^j.
    let (twos, after_two) = q.factor_out(2);
    let (fives, remaining) = after_two.factor_out(5);
    if remaining == BigInt::from_u64(1) {
        // Scale the numerator so the denominator becomes 10^max(i, j).
        let (coeff, scale) = if twos >= fives {
            (signed_p.mul(&pow_small(5, twos - fives)), i64::from(twos))
        } else {
            (signed_p.mul(&pow_small(2, fives - twos)), i64::from(fives))
        };
        return decimal_number(coeff, scale);
    }
    // No finite decimal expansion exists, so the exact law has no answer to give and the quotient rounds into binary64
    // — the same crossing the contagion path makes, so it is counted the same way and computed the same way: each
    // ORIGINAL operand to its nearest double, one IEEE division.
    // Converting the operands rather than the reduced rational is what makes the result byte for byte: the division
    // runs in doubles.
    //
    // The zero divisor left above, so this division cannot be by zero.
    resources.note_precision_boundary();
    Ok(float_result(left.to_f64() / right.to_f64()))
}

/// `%`: truncate both operands to exact integers, then integer-modulo with the dividend's sign. The truncation happens
/// before the divisor-zero check.
///
/// The exact law is OUTPUT-sized, never intermediate-sized: a negative-scale decimal dividend is `coeff * 10^k`, and
/// the remainder reduces it modulo the truncated divisor by modular exponentiation — the `10^k` magnitude is never
/// built, so a retained literal `1e100000000 % 2` answers `0` instantly instead of materializing a 10^8-digit dividend
/// for it. The only materializations left are the ANSWER itself (a huge divisor or dividend means a divisor- or
/// dividend-sized remainder, which the exact law must answer), and those are bounded by the big-integer digit ceiling
/// — the ceiling's refusal is this operator's numeric-range error; the request ledger is not consulted.
#[allow(
    clippy::too_many_lines,
    reason = "one arm per operand-scale pairing: the exact remainder law is read as a single table"
)]
fn remainder(left: &Num, right: &Num, resources: &jqf_resource::ResourceContext<'_>) -> Result<Number, NumError> {
    // The NaN law: a NaN operand makes the WHOLE `%` a NaN number — checked BEFORE the divisor-zero test, so `nan %
    // 0` is `null`, never a division error.
    if matches!(left, Num::Float(value) if value.is_nan()) || matches!(right, Num::Float(value) if value.is_nan()) {
        return Ok(Number::float(jqf_data::Float::new(f64::NAN)));
    }
    // A float operand keeps the DIRECT truncate-then-divide law (the saturating float-to-integer cast); the exact views
    // below are for the `Int`/`Dec` pairings only.
    if matches!(left, Num::Float(_)) || matches!(right, Num::Float(_)) {
        let dividend = left.truncate_to_integer().ok_or(NumError::NumericRange)?;
        let divisor = right.truncate_to_integer().ok_or(NumError::NumericRange)?;
        let (_, rem) = dividend.div_rem(&divisor).ok_or(NumError::RemainderByZero)?;
        return integer_number(&rem);
    }
    let (left_coeff, left_scale) = match left {
        Num::Int(value) => (value, 0),
        Num::Dec { coeff, scale } => (coeff, *scale),
        Num::Float(_) => unreachable!("float handled above"),
    };
    let (right_coeff, right_scale) = match right {
        Num::Int(value) => (value, 0),
        Num::Dec { coeff, scale } => (coeff, *scale),
        Num::Float(_) => unreachable!("float handled above"),
    };
    let negative = left_coeff.is_negative();
    let left_mag = left_coeff.abs();
    let right_mag = right_coeff.abs();
    // The divisor truncates to zero when its fraction is below one (`% 0.5` truncates the divisor to zero) or its
    // coefficient is zero. A negative-scale divisor is `b * 10^j` and is never materialized to answer this.
    let divisor_zero = if right_scale > 0 {
        match right.truncate_to_integer() {
            Some(value) => value.is_zero(),
            None => return Err(NumError::NumericRange),
        }
    } else {
        right_mag.is_zero()
    };
    if divisor_zero {
        return Err(NumError::RemainderByZero);
    }
    let rem = if left_scale > 0 {
        // The dividend is a bounded fraction: its truncation is at most coefficient-sized.
        let dividend = left.truncate_to_integer().ok_or(NumError::NumericRange)?.abs();
        if dividend.is_zero() {
            BigInt::zero()
        } else if right_scale <= 0 {
            // The divisor is `b * 10^j`: a dividend with fewer digits than the divisor's magnitude IS the remainder (it
            // never reaches the divisor); otherwise the gap is bounded by the dividend's own digits and the divisor
            // materializes directly.
            let right_power = right_scale.unsigned_abs();
            if usize::try_from(right_power)
                .map_or(true, |power| dividend.digit_count() < right_mag.digit_count() + power)
            {
                dividend
            } else {
                let divisor = right_mag
                    .mul_pow10(usize::try_from(right_power).map_err(|_| NumError::NumericRange)?)
                    .ok_or(NumError::NumericRange)?;
                dividend.div_rem(&divisor).ok_or(NumError::Allocation)?.1
            }
        } else {
            let divisor = right.truncate_to_integer().ok_or(NumError::NumericRange)?;
            dividend.div_rem(&divisor).ok_or(NumError::RemainderByZero)?.1
        }
    } else {
        // The dividend is `a * 10^k` with k = -sa — the huge-magnitude arm.
        let left_power = left_scale.unsigned_abs();
        if left_mag.is_zero() {
            BigInt::zero()
        } else if right_scale > 0 {
            // A bounded divisor: reduce the huge dividend modulo it.
            let divisor = right.truncate_to_integer().ok_or(NumError::NumericRange)?.abs();
            pow10_mul_mod(&left_mag, left_power, &divisor).ok_or(NumError::Allocation)?
        } else {
            // A negative-scale divisor is `b * 10^j`; factor the shared power of ten out of the modulus instead of
            // materializing either magnitude.
            let right_power = right_scale.unsigned_abs();
            if left_power >= right_power {
                // rem = 10^j * ((a * 10^(k-j)) mod b): the reduction runs against the divisor's COEFFICIENT.
                let reduced_rem =
                    pow10_mul_mod(&left_mag, left_power - right_power, &right_mag).ok_or(NumError::Allocation)?;
                if reduced_rem.is_zero() {
                    BigInt::zero()
                } else {
                    shifted_remainder(&reduced_rem, right_power, resources)?
                }
            } else {
                // rem = 10^k * (a mod (b * 10^(j-k))). The reduced modulus is coefficient-bounded exactly when `a >= b
                // * 10^(j-k)`, which the digit counts decide without materializing anything.
                let gap = right_power - left_power;
                let reduced_rem = match usize::try_from(gap) {
                    Ok(gap) if gap <= left_mag.digit_count() => {
                        let reduced_modulus = right_mag.mul_pow10(gap).ok_or(NumError::NumericRange)?;
                        left_mag.div_rem(&reduced_modulus).ok_or(NumError::Allocation)?.1
                    }
                    // `a < b * 10^gap`: the remainder is the dividend itself.
                    _ => left_mag.clone(),
                };
                if reduced_rem.is_zero() {
                    BigInt::zero()
                } else {
                    shifted_remainder(&reduced_rem, left_power, resources)?
                }
            }
        }
    };
    let rem = if negative { rem.negate() } else { rem };
    integer_number(&rem)
}

/// `(coeff * 10^power) mod modulus` for NON-NEGATIVE operands, via repeated squaring — the `10^power` magnitude is
/// never built, so a retained literal's whole-`i64` scale reduces in ~63 squarings against a coefficient-sized modulus.
/// `modulus` must be non-zero.
fn pow10_mul_mod(coeff: &BigInt, power: u64, modulus: &BigInt) -> Option<BigInt> {
    let coeff_mod = coeff.div_rem(modulus)?.1;
    let mut result = BigInt::from_u64(1);
    let mut base = BigInt::from_u64(10).div_rem(modulus)?.1;
    let mut remaining = power;
    while remaining != 0 {
        if remaining & 1 == 1 {
            result = result.mul(&base).div_rem(modulus)?.1;
        }
        base = base.mul(&base).div_rem(modulus)?.1;
        remaining >>= 1;
    }
    Some(coeff_mod.mul(&result).div_rem(modulus)?.1)
}

/// Materializes `r * 10^power` — a remainder whose magnitude is genuinely `power + digit_count(r)` digits, which the
/// exact law must answer — bounded by the big-integer digit ceiling (`mul_pow10` declines past it), so an
/// output-sized remainder cannot grow unbounded. The request ledger is not consulted on this path. `r` must be
/// non-zero.
fn shifted_remainder(
    r: &BigInt,
    power: u64,
    _resources: &jqf_resource::ResourceContext<'_>,
) -> Result<BigInt, NumError> {
    let power = usize::try_from(power).map_err(|_| NumError::NumericRange)?;
    r.mul_pow10(power).ok_or(NumError::NumericRange)
}

/// Truncates `coeff * 10^-scale` toward zero to an exact integer, or `None` when the truncated value cannot be
/// materialized (the `%` operand rule's defensive out-of-range class).
///
/// A magnitude below one truncates to zero and is decided WITHOUT building the `10^scale` divisor — a retained
/// literal with a whole-`i64` scale (`1e-9223372036854775807 % 1` is `0`) would otherwise materialize an exabyte
/// divisor to divide by it.
fn truncate_decimal(coeff: &BigInt, scale: i64) -> Option<BigInt> {
    if scale <= 0 {
        // Checked negation, not a plain `-`: the scale reaches the whole `i64` range through pinned spellings
        // (`1e-9223372036854775807`), and negating `i64::MIN` overflows.
        let exponent = i128::from(scale).checked_neg()?;
        let power = usize::try_from(exponent).ok()?;
        return coeff.mul_pow10(power);
    }
    if usize::try_from(scale).unwrap_or(usize::MAX) > coeff.digit_count() {
        return Some(BigInt::zero());
    }
    let divisor = BigInt::pow10(usize::try_from(scale).ok()?)?;
    coeff.div_rem(&divisor).map(|(quotient, _)| quotient)
}

/// Truncates a finite binary64 value toward zero without `std` (`f64::trunc` is a `std` intrinsic). Magnitudes at or
/// above `2^53` are already integral in binary64, so they pass through; smaller magnitudes truncate through the
/// well-defined saturating float-to-integer cast.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "truncation toward zero is the intent; magnitudes below 2^53 round-trip through i64 \
              exactly, and larger ones return unchanged above"
)]
pub fn trunc_toward_zero(value: f64) -> f64 {
    if !value.is_finite() || value.abs() >= 9_007_199_254_740_992.0 {
        return value;
    }
    (value as i64) as f64
}

/// `floor` for binary64 without `std`: truncate toward zero, then step DOWN when the truncation rounded a negative
/// value up.
///
/// It lives here, beside [`trunc_toward_zero`], because the slice bound law needs it in two places that must not drift:
/// the runtime resolver in [`crate::exec`] and the lower-time normalization on the engine's `SliceBounds`.
pub fn floor_binary64(value: f64) -> f64 {
    let truncated = trunc_toward_zero(value);
    if truncated > value { truncated - 1.0 } else { truncated }
}

/// `ceil` for binary64 without `std`: truncate toward zero, then step UP when the truncation rounded a positive value
/// down. Twin of [`floor_binary64`].
pub fn ceil_binary64(value: f64) -> f64 {
    let truncated = trunc_toward_zero(value);
    if truncated < value { truncated + 1.0 } else { truncated }
}

/// `base^exp` for the small bases 2 and 5 used by the terminating-decimal rescale.
fn pow_small(base: u8, exp: u32) -> BigInt {
    let base = BigInt::from_u64(u64::from(base));
    let mut result = BigInt::from_u64(1);
    for _ in 0..exp {
        result = result.mul(&base);
    }
    result
}

/// The non-negative digit gap `target - lower` (both scales, `target >= lower`) as a `usize` shift amount, erroring on
/// an absurd magnitude.
fn scale_gap(target: i64, lower: i64) -> Result<usize, NumError> {
    let gap = i128::from(target) - i128::from(lower);
    usize::try_from(gap).map_err(|_| NumError::NumericRange)
}

fn usize_pow(exponent: i128) -> Result<usize, NumError> {
    usize::try_from(exponent).map_err(|_| NumError::NumericRange)
}

/// Wraps a binary64 value, FINITE OR NOT, into an owned [`Number`].
///
/// Arithmetic never refuses a non-finite result: `nan + 1`, `infinite * 2` and `infinite - infinite` all complete and
/// publish, and the render path ([`jqf_data::number::text::format_binary64`], shared by the JSON encoder and the
/// error-message renderer) spells them the reference way — a NaN as the `null` literal, an infinity clamped to
/// `±1.7976931348623157e+308`. Rejecting them here was the numeric-range error, which made the most basic operation
/// diverge (`nan + 1` raised where the answer is `null`).
/// `Number::float` stores the exact IEEE bits, so NaN payloads and signed zero stay stable data exactly as the
/// `nan`/`infinite` literals store theirs.
fn float_result(value: f64) -> Number {
    Number::float(jqf_data::Float::new(value))
}

/// Builds an owned `Integer` number from a big integer.
fn integer_number(value: &BigInt) -> Result<Number, NumError> {
    let integer = Integer::from_canonical(value.to_decimal_string()).map_err(|_| NumError::Allocation)?;
    Number::try_integer_unaccounted(integer).map_err(|_| NumError::Allocation)
}

/// Builds a canonical number from a (possibly non-canonical) coefficient and scale via
/// [`Decimal::from_parts_normalizing`] (which strips trailing zeros).
///
/// An integral result — a magnitude with no fractional part — becomes an exact `Integer`. That includes a
/// coefficient that normalizes to scale zero (`2.5 * 2` → `5`, `0.5 + 0.5` → `1`) AND a coefficient that normalizes
/// to a NEGATIVE scale (`1000 * 0.1` has coefficient `1` at scale `-2`, i.e. the value `100`), provided the integer
/// falls inside the positional rendering window.
///
/// The negative-scale branch exists because a COMPUTED integral value renders positionally while `decpt <= ndigits +
/// 15` — so `1000 * 0.1` is `100` and `498000.0 * 1` is `498000`, not the scientific spellings (`1E+2`, `4.98E+5`)
/// that `DecimalText` derives once the coefficient's trailing zeros normalize the scale negative. A result OUTSIDE that
/// window keeps its `Decimal` and still renders scientifically (`1e16 * 1` → `1E+16`, `1e308 * 10` → `1E+309`),
/// matching the reference transition. A LITERAL never reaches here — it is materialized through the retained-scale
/// path — so `1e2 | tostring` keeps its `1E+2` scientific form.
///
/// Every other result keeps its retained coefficient as a `Decimal`, so the codec renders it exactly as the scientific
/// form would (`0.3`, and the out-of-plain-range `1E+309`).
#[allow(
    clippy::needless_pass_by_value,
    reason = "the coefficient is a computed result consumed logically at the canonical-storage boundary"
)]
fn decimal_number(coeff: BigInt, scale: i64) -> Result<Number, NumError> {
    let integer = Integer::from_canonical(coeff.to_decimal_string()).map_err(|_| NumError::Allocation)?;
    let decimal = Decimal::from_parts_normalizing(integer, scale).map_err(|_| NumError::NumericRange)?;
    // A normalized NEGATIVE scale means the value is integral (`coeff * 10^n`).
    // Render positionally as an exact Integer when the positional window holds (`adjusted <= count + 14`, i.e. `decpt
    // <= ndigits + 15`); otherwise keep the Decimal so `DecimalText` still renders scientific (`1E+16`, `1E+309`).
    if decimal.scale() < 0 {
        let (negative, digits) = match decimal.coefficient().as_str().strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, decimal.coefficient().as_str()),
        };
        let count = i128::try_from(digits.len()).map_err(|_| NumError::NumericRange)?;
        let exponent = -i128::from(decimal.scale());
        let adjusted = exponent + count - 1;
        if adjusted <= count + 14 {
            let mut text = String::new();
            if negative {
                text.push('-');
            }
            text.push_str(digits);
            for _ in 0..exponent {
                text.push('0');
            }
            let value = Integer::from_canonical(text).map_err(|_| NumError::Allocation)?;
            return Number::try_integer_unaccounted(value).map_err(|_| NumError::Allocation);
        }
    } else if decimal.scale() == 0 {
        let value =
            Integer::from_canonical(String::from(decimal.coefficient().as_str())).map_err(|_| NumError::Allocation)?;
        return Number::try_integer_unaccounted(value).map_err(|_| NumError::Allocation);
    }
    Number::try_decimal_unaccounted(decimal).map_err(|_| NumError::Allocation)
}

#[cfg(test)]
#[allow(
    clippy::float_cmp,
    reason = "float results are asserted bit-exact against the same IEEE op the engine performs"
)]
mod tests {
    use super::{ArithOp, NumError, compute_number};
    use alloc::string::String;
    use jqf_data::{Number, NumberCategory};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    /// One unlimited request ledger (a computed number charges its own payload).
    fn ledger() -> ResourceContext<'static> {
        let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let account = RequestAccount::try_new(limits).expect("test account");
        let work = WorkMeter::try_new_v1(1).expect("test work meter");
        ResourceContext::new(account, &CONTROL, work).expect("test ledger")
    }

    fn number(spelling: &str) -> Number {
        Number::try_json_literal(spelling).expect("literal")
    }

    /// Computes and renders a result as `(category, coefficient/text, scale)` for exact operands, or the float bits
    /// path for floats.
    fn eval(op: ArithOp, left: &str, right: &str) -> Number {
        compute_number(op, &number(left), &number(right), &ledger()).expect("computes")
    }

    fn integer_of(number: &Number) -> String {
        assert_eq!(number.category(), NumberCategory::Integer);
        String::from(number.to_integer().expect("integer").as_str())
    }

    #[test]
    fn float_contagion_counts_a_precision_boundary_event() {
        // A float operand makes the whole op a binary64 computation; the request-scoped counter records each such
        // boundary so `--diagnostics` can say a pipeline went fuzzy. Exact paths never increment it.
        let resources = ledger();
        let one_float = Number::float(jqf_data::Float::new(1.0));
        let _ = compute_number(ArithOp::Add, &number("0.1"), &one_float, &resources).expect("computes");
        assert_eq!(resources.precision_boundary_events(), 1);
        let _ = compute_number(ArithOp::Multiply, &number("0.1"), &one_float, &resources).expect("computes");
        assert_eq!(resources.precision_boundary_events(), 2);
    }

    /// A category-tagged rendering of any number, for asserting that two numbers are the same one without knowing which
    /// category they are.
    fn render_number(number: &Number) -> String {
        match number.category() {
            NumberCategory::Integer => integer_of(number),
            NumberCategory::Decimal => {
                let (coefficient, scale) = decimal_of(number);
                alloc::format!("d{coefficient}e{scale}")
            }
            NumberCategory::Float => {
                alloc::format!("f{:?}", number.as_float().expect("float").get())
            }
        }
    }

    fn decimal_of(number: &Number) -> (String, i64) {
        assert_eq!(number.category(), NumberCategory::Decimal);
        let decimal = number.as_decimal().expect("decimal");
        (String::from(decimal.coefficient().as_str()), decimal.scale())
    }

    #[test]
    fn integer_addition_stays_integer() {
        assert_eq!(integer_of(&eval(ArithOp::Add, "1", "2")), "3");
        assert_eq!(integer_of(&eval(ArithOp::Add, "50", "50")), "100");
        assert_eq!(integer_of(&eval(ArithOp::Subtract, "2", "5")), "-3");
        assert_eq!(
            integer_of(&eval(ArithOp::Add, "9007199254740993", "0")),
            "9007199254740993"
        );
    }

    #[test]
    fn exact_decimal_addition_is_not_double() {
        // 0.1 + 0.2 is exactly 0.3 (the double path gives 0.30000000000000004).
        assert_eq!(decimal_of(&eval(ArithOp::Add, "0.1", "0.2")), ("3".into(), 1));
        // 1.50 + 0 canonicalizes to 1.5 (retained spelling is a literal property).
        assert_eq!(decimal_of(&eval(ArithOp::Add, "1.50", "0")), ("15".into(), 1));
    }

    #[test]
    fn multiply_canonicalizes_integral_results() {
        // An integral decimal-path result is an Integer.
        assert_eq!(integer_of(&eval(ArithOp::Multiply, "2.5", "2")), "5");
        assert_eq!(integer_of(&eval(ArithOp::Multiply, "100.0", "0.010")), "1");
        // Exact ring closure: 1e308 * 10 is 1E+309 (Decimal), never a range error.
        assert_eq!(decimal_of(&eval(ArithOp::Multiply, "1e308", "10")), ("1".into(), -309));
    }

    #[test]
    fn computed_negative_scale_result_is_positional_within_the_window() {
        // A computed value whose trailing zeros normalize the scale negative is INTEGRAL and renders as an exact
        // Integer inside the positional window (decpt <= ndigits + 15), instead of the scientific 1E+2 / 4.98E+5
        // spellings.
        assert_eq!(integer_of(&eval(ArithOp::Multiply, "1000", "0.1")), "100");
        assert_eq!(integer_of(&eval(ArithOp::Multiply, "0.1", "1000")), "100");
        assert_eq!(integer_of(&eval(ArithOp::Add, "99.5", "0.5")), "100");
        assert_eq!(integer_of(&eval(ArithOp::Multiply, "498000.0", "1")), "498000");
        assert_eq!(integer_of(&eval(ArithOp::Multiply, "1e15", "1")), "1000000000000000");
        assert_eq!(integer_of(&eval(ArithOp::Multiply, "9.99e15", "1")), "9990000000000000");
    }

    #[test]
    fn computed_negative_scale_stays_scientific_outside_the_window() {
        // Beyond the positional window the Decimal is retained, so it still renders in scientific form (matching the
        // reference transition).
        assert_eq!(decimal_of(&eval(ArithOp::Multiply, "1e16", "1")), ("1".into(), -16));
        assert_eq!(decimal_of(&eval(ArithOp::Multiply, "1e20", "1")), ("1".into(), -20));
        assert_eq!(decimal_of(&eval(ArithOp::Multiply, "1e308", "10")), ("1".into(), -309));
    }

    #[test]
    fn exact_subtraction_of_beyond_range_decimals_is_zero() {
        // 1e400 - 1e400 is exactly 0 (jq's double gives inf - inf = nan -> null).
        assert_eq!(integer_of(&eval(ArithOp::Subtract, "1e400", "1e400")), "0");
    }

    #[test]
    fn division_classifies_integer_decimal_and_float() {
        assert_eq!(integer_of(&eval(ArithOp::Divide, "10", "5")), "2");
        assert_eq!(decimal_of(&eval(ArithOp::Divide, "10", "4")), ("25".into(), 1));
        // A quotient with no finite decimal expansion leaves the exact law and rounds into binary64. Each expectation
        // below is jq's own answer for the same program (`jq -n '1/3'`, `jq -n '22/7'`, `jq -n '[1,2,4] | add /
        // length'`), which is the point of the arm.
        for (left, right, expected) in [
            ("1", "3", 0.333_333_333_333_333_3_f64),
            ("22", "7", 3.142_857_142_857_143_f64),
            ("7", "3", 2.333_333_333_333_333_5_f64),
        ] {
            let quotient = compute_number(ArithOp::Divide, &number(left), &number(right), &ledger())
                .expect("a non-terminating quotient rounds, it does not refuse");
            assert_eq!(
                quotient.category(),
                NumberCategory::Float,
                "{left}/{right} must leave the exact law"
            );
            assert_eq!(quotient.as_float().expect("float").get(), expected, "{left}/{right}");
        }
    }

    /// The crossing is COUNTED. A non-terminating division takes exact operands out of the exact number law, which is
    /// the event `--diagnostics`' precision-boundary counter exists to surface; a silent crossing would make that
    /// counter a liar about the case a user is most likely to hit.
    #[test]
    fn a_rounded_quotient_counts_a_precision_boundary_event() {
        let resources = ledger();
        assert_eq!(resources.precision_boundary_events(), 0);
        let _ = compute_number(ArithOp::Divide, &number("1"), &number("3"), &resources).expect("rounds");
        assert_eq!(resources.precision_boundary_events(), 1);
        // A TERMINATING quotient stays inside the exact law and counts nothing.
        let _ = compute_number(ArithOp::Divide, &number("10"), &number("4"), &resources).expect("exact");
        assert_eq!(resources.precision_boundary_events(), 1);
    }

    #[test]
    fn float_contagion_returns_a_float() {
        // A FLOAT operand still contaminates the whole op: the exact decimal meets the float and the result is the
        // binary64 product.
        let float = Number::float(jqf_data::Float::new(1.5));
        let product = compute_number(ArithOp::Multiply, &float, &number("2"), &ledger()).expect("computes");
        assert_eq!(product.category(), NumberCategory::Float);
        assert_eq!(product.as_float().expect("float").get(), 3.0);
    }

    #[test]
    fn a_non_finite_result_is_a_value_not_a_range_error() {
        // Arithmetic never refuses a non-finite result: `nan + 1` prints `null` and `infinite * 2` prints the clamped
        // widest binary64. This is the law the pre-fix numeric-range error violated.
        let nan = Number::float(jqf_data::Float::new(f64::NAN));
        let infinite = Number::float(jqf_data::Float::new(f64::INFINITY));
        let neg_infinite = Number::float(jqf_data::Float::new(f64::NEG_INFINITY));
        for (op, left, right, expect_nan, expect_infinite) in [
            (ArithOp::Add, &nan, &number("1"), true, false),
            (ArithOp::Add, &infinite, &number("1"), false, true),
            (ArithOp::Subtract, &infinite, &infinite, true, false),
            (ArithOp::Multiply, &infinite, &number("0"), true, false),
            (ArithOp::Multiply, &neg_infinite, &number("2"), false, true),
            (ArithOp::Divide, &infinite, &infinite, true, false),
            (ArithOp::Divide, &infinite, &number("2"), false, true),
        ] {
            let result = compute_number(op, left, right, &ledger()).expect("propagates");
            let float = result.as_float().expect("non-finite result is a float");
            assert_eq!(float.get().is_nan(), expect_nan, "{op:?} {left:?} {right:?}");
            assert_eq!(float.get().is_infinite(), expect_infinite, "{op:?} {left:?} {right:?}");
        }
    }

    #[test]
    fn remainder_truncates_and_takes_the_dividend_sign() {
        assert_eq!(integer_of(&eval(ArithOp::Remainder, "10.9", "3")), "1");
        assert_eq!(integer_of(&eval(ArithOp::Remainder, "-10", "3")), "-1");
        assert_eq!(integer_of(&eval(ArithOp::Remainder, "10", "-3")), "1");
        // Exact modulo diverges from the double truncation here.
        assert_eq!(integer_of(&eval(ArithOp::Remainder, "9007199254740993", "2")), "1");
    }

    #[test]
    fn divide_and_remainder_by_zero_are_distinct_classes() {
        assert_eq!(
            compute_number(ArithOp::Divide, &number("1"), &number("0"), &ledger()).unwrap_err(),
            NumError::DivideByZero
        );
        assert_eq!(
            compute_number(ArithOp::Remainder, &number("1"), &number("0"), &ledger()).unwrap_err(),
            NumError::RemainderByZero
        );
        // The divisor truncates to zero before the remainder-zero check.
        assert_eq!(
            compute_number(ArithOp::Remainder, &number("5"), &number("0.5"), &ledger()).unwrap_err(),
            NumError::RemainderByZero
        );
    }

    /// Float-infinity `%` rides the Rust saturating float-to-int64 cast, and these rows PIN that answer. The reference
    /// double-to-int64 conversion is UB on an out-of-range float and platform-unstable (saturating on arm64 clang,
    /// `LLONG_MIN` on x86-64), so nothing else pins these. The compat corpus must never carry a `%`-with-infinity row
    /// that disagrees (none does today; the corpus's non-finite block excludes `%` by design).
    #[test]
    fn float_infinity_remainder_pins_the_saturating_cast() {
        let infinite = Number::float(jqf_data::Float::new(f64::INFINITY));
        let neg_infinite = Number::float(jqf_data::Float::new(f64::NEG_INFINITY));
        let nan = Number::float(jqf_data::Float::new(f64::NAN));
        let two = number("2");
        let zero = number("0");

        // `inf as i64` saturates to i64::MAX, so `infinite % 2` answers `9223372036854775807 % 2` = 1.
        assert_eq!(
            integer_of(&compute_number(ArithOp::Remainder, &infinite, &two, &ledger()).expect("computes")),
            "1"
        );
        // `-inf as i64` saturates to i64::MIN, an even value: remainder 0.
        assert_eq!(
            integer_of(&compute_number(ArithOp::Remainder, &neg_infinite, &two, &ledger()).expect("computes")),
            "0"
        );
        // The cast-site probes: `LLONG_MAX % LLONG_MAX` = 0 and `LLONG_MIN % LLONG_MAX` = -1.
        assert_eq!(
            integer_of(&compute_number(ArithOp::Remainder, &infinite, &infinite, &ledger()).expect("computes")),
            "0"
        );
        assert_eq!(
            integer_of(&compute_number(ArithOp::Remainder, &neg_infinite, &infinite, &ledger()).expect("computes")),
            "-1"
        );
        // A NaN operand makes the whole `%` a NaN — checked BEFORE the divisor-zero test, so `nan % 0` is a NaN too,
        // never a remainder-by-zero error.
        for (left, right) in [(&nan, &two), (&two, &nan), (&nan, &zero)] {
            let result = compute_number(ArithOp::Remainder, left, right, &ledger()).expect("computes");
            assert!(
                result.as_float().expect("float").get().is_nan(),
                "nan % operand must answer NaN"
            );
        }
    }

    /// The machine-integer fast path must be INDISTINGUISHABLE from the exact path: same bytes, same category, at the
    /// boundary and just past it.
    #[test]
    fn the_machine_integer_fast_path_agrees_with_the_exact_path() {
        let cases: &[(ArithOp, &str, &str, &str)] = &[
            (ArithOp::Add, "0", "1", "1"),
            (ArithOp::Add, "-0", "1", "1"),
            (ArithOp::Add, "-0", "0", "0"),
            (ArithOp::Add, "-1", "1", "0"),
            (ArithOp::Subtract, "1", "1", "0"),
            (ArithOp::Subtract, "0", "1", "-1"),
            (ArithOp::Multiply, "-3", "4", "-12"),
            (ArithOp::Multiply, "0", "-5", "0"),
            // At the i64 boundary the fast path still applies.
            (ArithOp::Add, "9223372036854775806", "1", "9223372036854775807"),
            (ArithOp::Subtract, "-9223372036854775807", "1", "-9223372036854775808"),
            // One step past it the fast path DECLINES and the exact path answers — which is the whole point: `+ - *`
            // stay ring-closed.
            (ArithOp::Add, "9223372036854775807", "1", "9223372036854775808"),
            (ArithOp::Subtract, "-9223372036854775808", "1", "-9223372036854775809"),
            (ArithOp::Multiply, "4611686018427387904", "4", "18446744073709551616"),
            // An operand outside i64 never enters the fast path.
            (
                ArithOp::Add,
                "12345678901234567890123456789",
                "1",
                "12345678901234567890123456790",
            ),
            // A decimal operand never enters it either, and still canonicalizes an integral result back to `Integer`.
            (ArithOp::Add, "1", "1.0", "2"),
        ];
        for (op, left, right, expected) in cases {
            assert_eq!(integer_of(&eval(*op, left, right)), *expected, "{left} {op:?} {right}");
        }
        // ...and a non-integral decimal result is untouched by the fast path.
        assert_eq!(decimal_of(&eval(ArithOp::Add, "1", "0.5")), ("15".into(), 1));
    }

    /// The FIVE operators over every pair of operand KINDS, in BOTH orders.
    ///
    /// The kinds are the ones the representation distinguishes — a machine integer (`i64` range, inline arm), a heap
    /// integer (past `i64`), a decimal, and a float — plus the retained `-0` an integer can carry. Each row pins the
    /// RESULT the arbitrary-precision path produces, so a machine operand that computed anything else, or a machine
    /// RESULT that rendered anything else, fails here rather than in a corpus row three commits later.
    #[test]
    fn arithmetic_is_blind_to_the_integer_storage_arm() {
        // (op, left, right, expected category tag, expected rendering) Category tag: 'i' integer text, 'd' decimal
        // (coefficient, scale), 'f' float.
        let integer_cases: &[(ArithOp, &str, &str, &str)] = &[
            // machine (+) machine, both orders.
            (ArithOp::Add, "7", "35", "42"),
            (ArithOp::Add, "35", "7", "42"),
            (ArithOp::Subtract, "7", "35", "-28"),
            (ArithOp::Subtract, "35", "7", "28"),
            (ArithOp::Multiply, "7", "-6", "-42"),
            (ArithOp::Multiply, "-6", "7", "-42"),
            (ArithOp::Divide, "42", "7", "6"),
            (ArithOp::Remainder, "42", "7", "0"),
            (ArithOp::Remainder, "7", "42", "7"),
            // machine (+) heap, both orders: the fast path DECLINES and the exact path answers; the result may land
            // back in machine range.
            (ArithOp::Add, "1", "9223372036854775808", "9223372036854775809"),
            (ArithOp::Add, "9223372036854775808", "1", "9223372036854775809"),
            (ArithOp::Subtract, "9223372036854775808", "1", "9223372036854775807"),
            (ArithOp::Subtract, "1", "9223372036854775808", "-9223372036854775807"),
            (ArithOp::Multiply, "2", "9223372036854775808", "18446744073709551616"),
            (ArithOp::Multiply, "9223372036854775808", "2", "18446744073709551616"),
            (ArithOp::Divide, "18446744073709551616", "2", "9223372036854775808"),
            (ArithOp::Divide, "18446744073709551616", "9223372036854775808", "2"),
            (ArithOp::Remainder, "9223372036854775809", "2", "1"),
            (ArithOp::Remainder, "2", "9223372036854775809", "2"),
            // heap (+) heap.
            (
                ArithOp::Add,
                "9223372036854775808",
                "9223372036854775808",
                "18446744073709551616",
            ),
            (ArithOp::Subtract, "9223372036854775808", "9223372036854775808", "0"),
        ];
        for (op, left, right, expected) in integer_cases {
            assert_eq!(integer_of(&eval(*op, left, right)), *expected, "{left} {op:?} {right}");
        }
    }

    /// The machine scaled-decimal fast path answers the exact path's number for every machine pairing it accepts, and
    /// computes NOTHING for every pairing it declines — floats (contagion's), heap integers, decimals past the
    /// machine coefficient range, and every overflow.
    #[test]
    fn machine_decimal_matches_the_exact_path() {
        let operands = [
            "0",
            "1",
            "-1",
            "42",
            "9007199254740993",
            "9223372036854775807",
            "0.5",
            "1.5",
            "0.01",
            "1.000",
            "0E5",
            "1e16",
            "1e15",
            // Past the machine coefficient range: the fast path declines.
            "12345678901234567890.5",
        ];
        for op in [
            ArithOp::Add,
            ArithOp::Subtract,
            ArithOp::Multiply,
            ArithOp::Divide,
            ArithOp::Remainder,
        ] {
            for left in operands {
                for right in operands {
                    let exact = super::apply(
                        op,
                        &super::Num::from_number(&number(left)).expect("exact operand"),
                        &super::Num::from_number(&number(right)).expect("exact operand"),
                        &ledger(),
                    );
                    let computed = compute_number(op, &number(left), &number(right), &ledger());
                    match (exact, computed) {
                        (Err(exact_error), Err(computed_error)) => {
                            assert_eq!(exact_error, computed_error, "{left} {op:?} {right}");
                        }
                        (Ok(exact), Ok(computed)) => {
                            assert_eq!(render_number(&computed), render_number(&exact), "{left} {op:?} {right}");
                        }
                        (exact, computed) => {
                            panic!("{left} {op:?} {right}: exact={exact:?} computed={computed:?}")
                        }
                    }
                }
            }
        }
    }

    /// The machine-decimal FOLD laws, spelling by spelling: same-scale alignment, scale-gap alignment, int/decimal
    /// mixing, canonical normalization (trailing zeros collapse, integral results become `Integer`), and the
    /// negative-scale positional window.
    #[test]
    fn machine_decimal_fold_laws() {
        // (op, left, right, category, rendering) Category 'i' = Integer text, 'd' = decimal (coefficient, scale).
        let cases: &[(ArithOp, &str, &str, char, &str)] = &[
            // Same-scale decimal arithmetic stays decimal.
            (ArithOp::Add, "0.1", "0.2", 'd', "d3e1"),
            (ArithOp::Add, "1.5", "2.25", 'd', "d375e2"),
            (ArithOp::Subtract, "1.0", "0.5", 'd', "d5e1"),
            (ArithOp::Multiply, "0.5", "0.5", 'd', "d25e2"),
            // Scale-gap alignment multiplies the smaller-scale coefficient.
            (ArithOp::Add, "0.1", "0.01", 'd', "d11e2"),
            (ArithOp::Subtract, "1.5", "0.25", 'd', "d125e2"),
            (ArithOp::Add, "0.01", "0.1", 'd', "d11e2"),
            // An integer is the scale-zero decimal: one path owns the mixing.
            (ArithOp::Add, "1", "0.001", 'd', "d1001e3"),
            (ArithOp::Add, "0.001", "1", 'd', "d1001e3"),
            (ArithOp::Multiply, "2.5", "2", 'i', "5"),
            // Canonical normalization: trailing zeros collapse, integral results become the exact Integer.
            (ArithOp::Add, "0.5", "0.5", 'i', "1"),
            (ArithOp::Subtract, "1.5", "1.5", 'i', "0"),
            (ArithOp::Multiply, "0.05", "0.2", 'd', "d1e2"),
            // The negative-scale positional window: `1e15 * 1` renders positionally, `1e16 * 1` stays the scientific
            // Decimal.
            (ArithOp::Multiply, "1e15", "1", 'i', "1000000000000000"),
            (ArithOp::Multiply, "1e16", "1", 'd', "d1e-16"),
            (ArithOp::Multiply, "1e16", "0.1", 'i', "1000000000000000"),
            // Overflow promotes to the exact path instead of wrapping.
            (
                ArithOp::Add,
                "9000000000000000000",
                "9000000000000000000",
                'i',
                "18000000000000000000",
            ),
            (
                ArithOp::Add,
                "0.1",
                "9000000000000000000",
                'd',
                "d90000000000000000001e1",
            ),
            (
                ArithOp::Multiply,
                "9000000000",
                "9000000000",
                'i',
                "81000000000000000000",
            ),
        ];
        for (op, left, right, category, expected) in cases {
            let number = eval(*op, left, right);
            let rendered = match category {
                'i' => integer_of(&number),
                'd' => {
                    let (coefficient, scale) = decimal_of(&number);
                    alloc::format!("d{coefficient}e{scale}")
                }
                _ => unreachable!(),
            };
            assert_eq!(rendered, *expected, "{left} {op:?} {right}");
        }
    }

    /// The in-place decimal update answers exactly what the borrowed form answers — including the pairings it
    /// declines, where the accumulator must come back untouched (the general law still owns it).
    #[test]
    fn machine_decimal_in_place_answers_what_the_borrowed_form_answers() {
        let operands = [
            "0",
            "1",
            "42",
            "9007199254740993",
            "9223372036854775807",
            "0.1",
            "1.5",
            "0.01",
            "1.000",
            "0E5",
            "1e16",
            "1e15",
            "12345678901234567890.5",
            "9223372036854775808",
        ];
        for op in [
            ArithOp::Add,
            ArithOp::Subtract,
            ArithOp::Multiply,
            ArithOp::Divide,
            ArithOp::Remainder,
        ] {
            for left in operands {
                for right in operands {
                    let mut target = number(left);
                    let updated = super::try_machine_decimal_in_place(op, &mut target, &number(right));
                    let borrowed = compute_number(op, &number(left), &number(right), &ledger());
                    let Ok(borrowed) = borrowed else {
                        assert!(!updated, "{left} {op:?} {right} answered a refusal");
                        continue;
                    };
                    if updated {
                        assert_eq!(
                            render_number(&target),
                            render_number(&borrowed),
                            "{left} {op:?} {right}"
                        );
                    } else {
                        assert_eq!(
                            render_number(&target),
                            render_number(&number(left)),
                            "{left} {op:?} {right} was disturbed by a decline"
                        );
                    }
                }
            }
        }
    }

    /// A SHARED decimal representation detaches rather than being written through, so the other handle keeps the value
    /// it had — the decimal twin of the integer in-place detach law.
    #[test]
    fn the_machine_decimal_in_place_update_detaches_a_shared_representation() {
        let original = number("0.5");
        let mut target = original.clone();
        assert!(super::try_machine_decimal_in_place(
            ArithOp::Add,
            &mut target,
            &number("0.5")
        ));
        assert_eq!(render_number(&target), "1", "0.5 + 0.5 canonicalizes to the integer 1");
        assert_eq!(render_number(&original), "d5e1", "the other handle moved");
    }

    /// The in-place update answers exactly what the borrowed form answers, for every operand pairing and every operator
    /// — including the pairings and operators it DECLINES, where the left operand must come back untouched.
    ///
    /// The two share [`machine_result`](super::machine_result), so this is not asserting two implementations against
    /// each other; it asserts that the ONE law reaches the same number through both doors, which is what lets
    /// `apply_owned` prefer the in-place door without a second semantics.
    #[test]
    fn the_in_place_update_answers_what_the_borrowed_form_answers() {
        let operands = [
            "0",
            "1",
            "-1",
            "42",
            "-42",
            "9007199254740993",
            "9223372036854775807",
            "-9223372036854775808",
            // Past the machine arm in both directions: the fast path declines.
            "9223372036854775808",
            "-9223372036854775809",
            // Not integers at all, so the fast path never owns them.
            "1.5",
            "1.000",
            "0E5",
        ];
        let ops = [
            ArithOp::Add,
            ArithOp::Subtract,
            ArithOp::Multiply,
            ArithOp::Divide,
            ArithOp::Remainder,
        ];
        for op in ops {
            for left in operands {
                for right in operands {
                    let mut target = number(left);
                    let updated = super::try_machine_integer_in_place(op, &mut target, &number(right));
                    // A divisor of zero has no borrowed answer to compare against; the fast path must have declined it
                    // too.
                    let borrowed = compute_number(op, &number(left), &number(right), &ledger());
                    let Ok(borrowed) = borrowed else {
                        assert!(!updated, "{left} {op:?} {right} answered a refusal");
                        continue;
                    };
                    if updated {
                        assert_eq!(integer_of(&target), integer_of(&borrowed), "{left} {op:?} {right}");
                    } else {
                        // A decline leaves the operand exactly as it was, so the general law still sees its own left
                        // operand.
                        assert_eq!(
                            render_number(&target),
                            render_number(&number(left)),
                            "{left} {op:?} {right} was disturbed by a decline"
                        );
                    }
                }
            }
        }
    }

    /// A SHARED representation detaches rather than being written through, so the other handle keeps the value it had.
    ///
    /// This is the only way the in-place door could ever be observable, and it is the case that closes it.
    #[test]
    fn the_in_place_update_detaches_a_shared_representation() {
        let original = number("41");
        let mut target = original.clone();
        assert!(super::try_machine_integer_in_place(
            ArithOp::Add,
            &mut target,
            &number("1")
        ));
        assert_eq!(integer_of(&target), "42");
        assert_eq!(integer_of(&original), "41", "the other handle moved");
    }

    /// The `i64` BOUNDARY rows of the same matrix: at the extremes the fast path applies and one step past it promotes,
    /// and both must render the exact digits. These mirror the corpus's five boundary `intdiff` rows.
    #[test]
    fn arithmetic_at_the_i64_boundary_promotes_without_wrapping() {
        let integer_cases: &[(ArithOp, &str, &str, &str)] = &[
            (ArithOp::Add, "9223372036854775806", "1", "9223372036854775807"),
            (ArithOp::Add, "9223372036854775807", "1", "9223372036854775808"),
            (ArithOp::Subtract, "-9223372036854775807", "1", "-9223372036854775808"),
            (ArithOp::Subtract, "-9223372036854775808", "1", "-9223372036854775809"),
            (ArithOp::Multiply, "-9223372036854775808", "1", "-9223372036854775808"),
            (ArithOp::Multiply, "4611686018427387904", "4", "18446744073709551616"),
            // 2^53 +/- 1: exactly where the double law stops agreeing.
            (ArithOp::Add, "9007199254740992", "1", "9007199254740993"),
            (ArithOp::Subtract, "9007199254740993", "1", "9007199254740992"),
            (ArithOp::Remainder, "9007199254740993", "2", "1"),
            // machine (+) decimal, both orders, with an integral result.
            (ArithOp::Add, "1", "1.0", "2"),
            (ArithOp::Add, "1.0", "1", "2"),
            (ArithOp::Multiply, "2.5", "2", "5"),
            (ArithOp::Multiply, "2", "2.5", "5"),
            (ArithOp::Remainder, "10.9", "3", "1"),
            (ArithOp::Remainder, "3", "10.9", "3"),
        ];
        for (op, left, right, expected) in integer_cases {
            assert_eq!(integer_of(&eval(*op, left, right)), *expected, "{left} {op:?} {right}");
        }
    }

    /// A retained `-0` integer operand is semantically zero in every operator, and every result carries the CANONICAL
    /// zero.
    ///
    /// It is the one integer the machine arm refuses, so it is the one operand that reaches arithmetic in the heap arm
    /// while its value is inside `i64` range — the exact case the fast path must still get right.
    #[test]
    fn a_retained_negative_zero_operand_computes_as_zero() {
        let negative_zero = Number::integer(jqf_data::Integer::from_canonical(String::from("-0")).expect("canonical"));
        let one = number("1");
        for (op, expected) in [(ArithOp::Add, "1"), (ArithOp::Multiply, "0"), (ArithOp::Remainder, "0")] {
            assert_eq!(
                integer_of(&compute_number(op, &negative_zero, &one, &ledger()).expect("computes")),
                expected,
                "-0 {op:?} 1"
            );
        }
        assert_eq!(
            integer_of(&compute_number(ArithOp::Subtract, &one, &negative_zero, &ledger()).expect("computes")),
            "1"
        );
        assert_eq!(
            compute_number(ArithOp::Divide, &one, &negative_zero, &ledger()).unwrap_err(),
            NumError::DivideByZero
        );
    }

    /// Float contagion, both orders and both integer arms: the machine operand's direct `value as f64` must land on the
    /// same double the `BigInt` decimal round trip produced. (A `0.5` literal is a retained-spelling DECIMAL, not a
    /// float, so the float operands are built directly.)
    #[test]
    fn float_contagion_is_blind_to_the_integer_storage_arm() {
        let negative_zero = Number::integer(jqf_data::Integer::from_canonical(String::from("-0")).expect("canonical"));
        let half = Number::float(jqf_data::Float::new(0.5));
        let float_cases: &[(ArithOp, &str, bool, f64)] = &[
            // (op, integer operand, integer-on-the-left, expected)
            (ArithOp::Add, "1", true, 1.5),
            (ArithOp::Add, "1", false, 1.5),
            (ArithOp::Subtract, "1", true, 0.5),
            (ArithOp::Subtract, "1", false, -0.5),
            (ArithOp::Multiply, "3", true, 1.5),
            (ArithOp::Multiply, "3", false, 1.5),
            (ArithOp::Divide, "3", true, 6.0),
            (ArithOp::Divide, "3", false, 0.5_f64 / 3.0_f64),
            // The heap arm converts through its exact operand, unchanged.
            (
                ArithOp::Multiply,
                "9223372036854775808",
                true,
                9_223_372_036_854_775_808.0_f64 * 0.5,
            ),
            // The `i64` extremes convert exactly the way their spellings do.
            (
                ArithOp::Add,
                "9223372036854775807",
                true,
                9_223_372_036_854_775_807.0_f64 + 0.5,
            ),
            (
                ArithOp::Add,
                "-9223372036854775808",
                true,
                -9_223_372_036_854_775_808.0_f64 + 0.5,
            ),
        ];
        for (op, integer, integer_left, expected) in float_cases {
            let integer = number(integer);
            let (left, right) = if *integer_left {
                (&integer, &half)
            } else {
                (&half, &integer)
            };
            let result = compute_number(*op, left, right, &ledger()).expect("computes");
            assert_eq!(result.category(), NumberCategory::Float, "{op:?} {integer_left}");
            assert_eq!(
                result.as_float().expect("float").get(),
                *expected,
                "{op:?} {integer_left}"
            );
        }

        // `%` is NOT contagious: it truncates both operands to exact integers first, so a float operand still yields an
        // Integer.
        let ten_and_a_half = Number::float(jqf_data::Float::new(10.5));
        assert_eq!(
            integer_of(
                &compute_number(ArithOp::Remainder, &ten_and_a_half, &number("3"), &ledger()).expect("computes")
            ),
            "1"
        );
        assert_eq!(
            integer_of(
                &compute_number(ArithOp::Remainder, &number("3"), &ten_and_a_half, &ledger()).expect("computes")
            ),
            "3"
        );

        // Contagion over a MACHINE operand must agree bit for bit with the exact operand path it replaced, for every
        // value in the matrix — and the retained `-0` must convert to POSITIVE zero, as the exact path does, not to
        // the `-0.0` its bytes would parse to.
        let one_float = Number::float(jqf_data::Float::new(1.0));
        for spelling in [
            "0",
            "1",
            "-1",
            "1000",
            "9007199254740993",
            "9223372036854775807",
            "-9223372036854775808",
            "9223372036854775808",
        ] {
            let reference: f64 = spelling.parse().expect("f64");
            let direct = compute_number(ArithOp::Multiply, &number(spelling), &one_float, &ledger()).expect("computes");
            assert_eq!(
                direct.as_float().expect("float").get(),
                reference * 1.0_f64,
                "contagion {spelling}"
            );
        }
        let contagious_negative_zero =
            compute_number(ArithOp::Multiply, &negative_zero, &one_float, &ledger()).expect("computes");
        assert!(
            !contagious_negative_zero
                .as_float()
                .expect("float")
                .get()
                .is_sign_negative(),
            "a retained -0 integer converts to POSITIVE zero under contagion"
        );
    }

    /// `/` and `%` must never take the fast path: their exactness, zero-divisor and truncation laws are the exact
    /// path's own.
    #[test]
    fn division_and_remainder_stay_on_the_exact_path() {
        assert_eq!(decimal_of(&eval(ArithOp::Divide, "1", "2")), ("5".into(), 1));
        assert_eq!(integer_of(&eval(ArithOp::Divide, "6", "3")), "2");
        assert_eq!(integer_of(&eval(ArithOp::Remainder, "7", "3")), "1");
        assert_eq!(
            compute_number(ArithOp::Divide, &number("1"), &number("0"), &ledger()).err(),
            Some(NumError::DivideByZero)
        );
        assert_eq!(
            compute_number(ArithOp::Remainder, &number("1"), &number("0"), &ledger()).err(),
            Some(NumError::RemainderByZero)
        );
    }

    /// A retained literal whose scale is the whole `i64` range must refuse a result it cannot materialize as a typed
    /// error — never an allocation panic — and must still answer the shapes the value law decides without
    /// materializing (`%` truncates a magnitude below one to zero).
    #[test]
    fn an_i64_scale_literal_refuses_cleanly_or_answers_exactly() {
        let tiny = number("1e-9223372036854775807");
        let huge = number("1e9223372036854775807");
        // The truncated dividend is zero, so the remainder is zero — the same answer the reference gives for the same
        // spelling.
        assert_eq!(
            integer_of(&compute_number(ArithOp::Remainder, &tiny, &number("1"), &ledger()).expect("truncates to zero")),
            "0"
        );
        // A sum/subtraction past any materialization is the defensive out-of-range class, never a panic.
        for op in [ArithOp::Add, ArithOp::Subtract] {
            assert_eq!(
                compute_number(op, &tiny, &number("1"), &ledger()).unwrap_err(),
                NumError::NumericRange,
                "{op:?}"
            );
        }
        assert_eq!(
            compute_number(ArithOp::Add, &huge, &number("1"), &ledger()).unwrap_err(),
            NumError::NumericRange
        );
        // Division past any materialization refuses (the exact quotient would be materialization-sized, unlike `%`'s
        // modular remainder).
        assert_eq!(
            compute_number(ArithOp::Divide, &number("1"), &tiny, &ledger()).unwrap_err(),
            NumError::NumericRange
        );
        // A close-scale pair still computes exactly (the larger scale wins).
        assert_eq!(
            decimal_of(
                &compute_number(ArithOp::Add, &tiny, &number("1e-9223372036854775806"), &ledger())
                    .expect("close scales align")
            ),
            ("11".into(), 9_223_372_036_854_775_807)
        );
        // A huge-scale dividend is now reduced modulo a small divisor WITHOUT materializing the `10^k` magnitude: the
        // exact remainder answers instead of the old defensive refusal, which existed only because the materialization
        // was the only path.
        assert_eq!(
            integer_of(
                &compute_number(ArithOp::Remainder, &huge, &number("1"), &ledger())
                    .expect("modular reduction answers exactly")
            ),
            "0"
        );
        // A huge-scale DIVISOR is factored the same way: `1 mod 10^k` is `1`, answered without materializing the
        // divisor's magnitude (the old defensive refusal existed only because materialization was the only path).
        assert_eq!(
            integer_of(&compute_number(ArithOp::Remainder, &number("1"), &huge, &ledger()).expect("factored divisor")),
            "1"
        );
    }

    /// `%` reduces a huge-scale decimal dividend modulo a small divisor WITHOUT materializing the `10^k` magnitude: the
    /// retained-literal denial-of-service shape (`1e100000000 % 2`) answers instantly, and a negative-scale DIVISOR is
    /// factored through its coefficient instead of materialized (`1e100000000 % 1e50000000` is `0`, never a 10^8-digit
    /// intermediate).
    #[test]
    fn huge_scale_remainder_reduces_modulo_without_materializing() {
        let resources = ledger();
        // The dividend is 10^100000000 — even, so the remainder is 0.
        assert_eq!(
            integer_of(
                &compute_number(ArithOp::Remainder, &number("1e100000000"), &number("2"), &resources)
                    .expect("modular remainder")
            ),
            "0"
        );
        assert_eq!(
            integer_of(
                &compute_number(ArithOp::Remainder, &number("1e1000000"), &number("2"), &resources)
                    .expect("modular remainder")
            ),
            "0"
        );
        // 10 ≡ 1 (mod 3), so 10^100000000 mod 3 is 1.
        assert_eq!(
            integer_of(
                &compute_number(ArithOp::Remainder, &number("1e100000000"), &number("3"), &resources)
                    .expect("modular remainder")
            ),
            "1"
        );
        // The divisor 10^50000000 divides 10^100000000: remainder 0, and the divisor's own magnitude is never built.
        assert_eq!(
            integer_of(
                &compute_number(
                    ArithOp::Remainder,
                    &number("1e100000000"),
                    &number("1e50000000"),
                    &resources
                )
                .expect("factored divisor")
            ),
            "0"
        );
        // The ordinary law is unchanged: truncation, dividend sign, and the divisor-zero class.
        assert_eq!(
            integer_of(
                &compute_number(ArithOp::Remainder, &number("-7"), &number("3"), &resources).expect("truncating law")
            ),
            "-1"
        );
        assert_eq!(
            integer_of(
                &compute_number(ArithOp::Remainder, &number("7"), &number("-3"), &resources).expect("truncating law")
            ),
            "1"
        );
        assert_eq!(
            integer_of(
                &compute_number(ArithOp::Remainder, &number("-5.5"), &number("2"), &resources).expect("truncating law")
            ),
            "-1"
        );
        assert_eq!(
            integer_of(
                &compute_number(ArithOp::Remainder, &number("1e-10"), &number("3"), &resources)
                    .expect("fraction truncates to zero")
            ),
            "0"
        );
        assert_eq!(
            compute_number(ArithOp::Remainder, &number("0.5"), &number("0.5"), &resources).err(),
            Some(NumError::RemainderByZero)
        );
    }

    /// Opposite-extreme scales: the machine path's alignment gap (`right_scale - left_scale`) overflows `i64` on this
    /// pairing, so both orderings must DECLINE to the exact path — whose digit ceiling refuses the aligned
    /// intermediate — instead of wrapping into a wrong answer or panicking a debug build on the subtraction.
    #[test]
    fn opposite_extreme_scales_decline_without_overflowing() {
        let resources = ledger();
        let huge = number("1e9223372036854775807");
        let tiny = number("1e-9223372036854775807");
        assert_eq!(
            compute_number(ArithOp::Add, &huge, &tiny, &resources).err(),
            Some(NumError::NumericRange)
        );
        assert_eq!(
            compute_number(ArithOp::Add, &tiny, &huge, &resources).err(),
            Some(NumError::NumericRange)
        );
        assert_eq!(
            compute_number(ArithOp::Subtract, &huge, &tiny, &resources).err(),
            Some(NumError::NumericRange)
        );
        // Multiplication ADDS scales, which cancels exactly on this pairing:
        // the machine path answers it without any gap at all.
        assert_eq!(
            integer_of(&compute_number(ArithOp::Multiply, &tiny, &huge, &ledger()).expect("scale addition is exact")),
            "1"
        );
    }
}
