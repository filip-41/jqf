//! Numbers: exact integers, exact base-ten decimals, and binary64.
//!
//! - [`Integer`] — arbitrary precision. A literal past 2^53 stays exact.
//! - [`Decimal`] — exact finite base-ten. A YAML `1.5` and a JSON `1.5`   are the same value.
//! - [`Float`] — IEEE-754 binary64. The only kind that keeps `.inf` /   `.nan`.
//!
//! [`Number`] is the owned wrapper. Construction does not charge a request ledger: fallible constructors are uncharged,
//! like all value constructors here.
//!
//! [`BigInt`] is scratch arithmetic the caller drives and charges. No [`Number`] holds one.

mod bigint;
mod decimal;
mod float;
mod integer;
mod text;

pub use bigint::BigInt;
pub use decimal::{Decimal, decimal_parts_to_f64};
pub use float::Float;
pub use integer::Integer;
pub use text::{DecimalText, format_binary64};

use alloc::string::String;
use core::fmt;
use triomphe::Arc;

// --------------------------------------------------------------------------- Shared public types
// ---------------------------------------------------------------------------

/// Integer, decimal, or float.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NumberCategory {
    /// Arbitrary-precision integer.
    Integer,
    /// Exact finite base-ten decimal.
    Decimal,
    /// IEEE-754 binary64 value.
    Float,
}

/// Why a number could not be built.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum NumericError {
    /// The spelling contains no digit characters.
    Empty,
    /// A character was not valid in the requested numeric representation.
    InvalidDigit,
    /// The decimal exponent is missing, malformed, or outside the parse range.
    InvalidExponent,
    /// An arithmetic operation required while normalizing overflowed.
    ExponentOverflow,
    /// Allocation failed while constructing canonical storage.
    Allocation,
}

impl fmt::Display for NumericError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "number contains no digits",
            Self::InvalidDigit => "number contains an invalid digit",
            Self::InvalidExponent => "number has an invalid exponent",
            Self::ExponentOverflow => "number exponent exceeds the supported range",
            Self::Allocation => "number allocation failed",
        })
    }
}

impl core::error::Error for NumericError {}

#[derive(Clone, Debug)]
enum NumberRepr {
    Integer(Integer),
    Decimal(Decimal),
    Float(Float),
}

impl NumberRepr {
    /// Whether this representation carries a minus. See [`Number::is_negative`].
    fn is_negative(&self) -> bool {
        match self {
            NumberRepr::Integer(value) => value.as_str().starts_with('-'),
            NumberRepr::Decimal(value) => value.coefficient().as_str().starts_with('-'),
            NumberRepr::Float(value) => value.get().is_sign_negative(),
        }
    }

    /// Same as [`Number::try_sign_stripped`] for a boxed payload. A magnitude that fits `i64` lands inline.
    fn try_sign_stripped(&self) -> Result<Number, NumericError> {
        match self {
            NumberRepr::Integer(value) => {
                Number::try_integer_unaccounted(Integer::from_canonical_ref(strip_sign(value.as_str()))?)
            }
            NumberRepr::Decimal(value) => {
                let coefficient = Integer::from_canonical_ref(strip_sign(value.coefficient().as_str()))?;
                Number::try_decimal_unaccounted(Decimal::from_literal_parts(coefficient, value.scale())?)
            }
            NumberRepr::Float(value) => Ok(Number::float(Float::new(value.get().abs()))),
        }
    }

    /// Whether this representation's VALUE is zero, read through each payload's own zero encoding: the canonical
    /// integer `0`, any scale's coefficient `0`, or a `0.0` magnitude.
    fn is_zero(&self) -> bool {
        match self {
            NumberRepr::Integer(value) => value.as_str() == "0",
            NumberRepr::Decimal(value) => value.coefficient().as_str() == "0",
            NumberRepr::Float(value) => value.get() == 0.0,
        }
    }

    /// Same as [`Number::try_negated`] for a boxed payload that is neither negative nor zero. The caller already
    /// handled those.
    fn try_negated(&self) -> Result<Number, NumericError> {
        match self {
            NumberRepr::Integer(value) => {
                Number::try_integer_unaccounted(Integer::from_canonical(prefix_minus(value.as_str())?)?)
            }
            NumberRepr::Decimal(value) => {
                let coefficient = Integer::from_canonical(prefix_minus(value.coefficient().as_str())?)?;
                Number::try_decimal_unaccounted(Decimal::from_literal_parts(coefficient, value.scale())?)
            }
            NumberRepr::Float(value) => Ok(Number::float(Float::new(-value.get()))),
        }
    }
}

/// The stored representation behind a [`Number`].
#[derive(Clone, Debug)]
enum NumberStorage {
    /// Inline machine integer — the computed-integer hot path. No spelling is stored: the canonical decimal renders
    /// from the value.
    Machine(i64),
    /// Inline binary64 — the float hot path. A float has no retained spelling (bare IEEE bits), so it needs no
    /// allocation; a boxed float exists only as the path-mode allocation-identity arm ([`Number::boxed_float`]).
    Float(Float),
    /// Decimals and big integers share one allocation because their retained spelling may differ from the canonical
    /// render and must survive. A boxed float rides here too — an allocation object with no spelling to preserve.
    Boxed(Arc<NumberRepr>),
}

// --------------------------------------------------------------------------- Number
// ---------------------------------------------------------------------------

/// Owned number: exact integer, exact finite decimal, or binary64.
///
/// A computed `i64` lives inline with no stored spelling. Everything else is one shared allocation. Clone of a boxed
/// number is a refcount bump. A boxed number may keep a source spelling that is not the canonical render (`-0`,
/// `1.000`).
///
/// No `PartialEq` or `Ord`. The caller that compares numbers owns that comparison.
#[derive(Clone, Debug)]
pub struct Number(NumberStorage);

impl Number {
    // --- Constructors ---

    /// Creates an integer number: inline when the value is a machine integer, boxed when it is arbitrary-precision (or
    /// a literal whose retained spelling must survive).
    #[must_use]
    pub fn integer(value: Integer) -> Self {
        match value.as_machine() {
            Some(machine) => Self(NumberStorage::Machine(machine)),
            // A Big integer boxes even when its retained spelling parses to an `i64` (a document `-0`): the spelling is
            // the value it carries.
            None => Self::boxed(Arc::new(NumberRepr::Integer(value))),
        }
    }

    /// Tries to create an integer number, inline for machine values and fallibly boxed for arbitrary-precision ones
    /// (allocation can fail).
    ///
    /// Unaccounted: no request ledger is charged (see the module doc).
    pub fn try_integer_unaccounted(value: Integer) -> Result<Self, NumericError> {
        match value.as_machine() {
            Some(machine) => Ok(Self(NumberStorage::Machine(machine))),
            None => Arc::try_new(NumberRepr::Integer(value))
                .map(Self::boxed)
                .map_err(|_| NumericError::Allocation),
        }
    }

    /// Creates an exact finite base-ten decimal number.
    #[must_use]
    pub fn decimal(value: Decimal) -> Self {
        Self::boxed(Arc::new(NumberRepr::Decimal(value)))
    }

    /// Tries to create an exact finite base-ten decimal number.
    ///
    /// Unaccounted: no request ledger is charged (see the module doc).
    pub fn try_decimal_unaccounted(value: Decimal) -> Result<Self, NumericError> {
        Arc::try_new(NumberRepr::Decimal(value))
            .map(Self::boxed)
            .map_err(|_| NumericError::Allocation)
    }

    /// The one place a boxed representation allocation is wrapped.
    fn boxed(repr: Arc<NumberRepr>) -> Self {
        Self(NumberStorage::Boxed(repr))
    }

    /// Creates a binary64 number, INLINE: the common float arm holds the bits in the enum and allocates nothing (a
    /// float never has a retained spelling — `Float` is bare IEEE bits).
    #[must_use]
    pub fn float(value: Float) -> Self {
        Self(NumberStorage::Float(value))
    }

    /// A BOXED binary64 — the float analogue of [`Integer::boxed_from_i64`]: an allocation identity for a computed or
    /// literal float, so the path register's allocation-identity check can reject it exactly as it rejects a computed
    /// integer or decimal. Every document and arithmetic float lands on the inline arm; only path mode builds this.
    #[must_use]
    pub fn boxed_float(value: Float) -> Self {
        Self::boxed(Arc::new(NumberRepr::Float(value)))
    }

    /// Whether this number is an INLINE binary64 — the float analogue of [`Self::as_machine`], which is the
    /// inline-int probe: both answer `true` only for the inline scalar arms, and a boxed float (the path-mode
    /// allocation-identity object) answers `false`.
    #[must_use]
    pub fn is_inline_float(&self) -> bool {
        matches!(self.0, NumberStorage::Float(_))
    }

    /// Exact number for a finite binary64's shortest-round-trip spelling.
    ///
    /// Built from significand digits and exponent, not via `to_string`. Zero is integer zero. An integral value is an
    /// integer (`1e21` → `1000000000000000000000`). A non-integral value is a decimal. Route non-finite values
    /// through [`Self::float`] first.
    ///
    /// Negative zero keeps its sign, the same `-0` [`Self::try_json_literal`] answers for the literal.
    pub fn try_from_shortest_f64(value: f64) -> Result<Self, NumericError> {
        let negative = value.is_sign_negative();
        let (digits, exponent) = text::shortest_round_trip_parts(value.abs()).ok_or(NumericError::InvalidDigit)?;
        let digits = digits.as_str();
        let mut accumulated: i64 = 0;
        for byte in digits.bytes() {
            accumulated = accumulated * 10 + i64::from(byte - b'0');
        }
        if accumulated == 0 {
            return if negative {
                Self::try_integer_unaccounted(Integer::from_canonical_ref("-0")?)
            } else {
                Ok(Self::integer(Integer::from_i64(0)))
            };
        }
        let ndigits = i64::try_from(digits.len()).map_err(|_| NumericError::ExponentOverflow)?;
        let scale = ndigits - i64::from(exponent + 1);
        if scale > 0 {
            // Non-integral: the coefficient is the significand digits.
            let coefficient = Integer::from_i64(if negative { -accumulated } else { accumulated });
            return Self::try_decimal_unaccounted(Decimal::from_parts(coefficient, scale)?);
        }
        // Integral: the value is the digits followed by `-scale` zeroes — a machine integer when that fits `i64`, a
        // big integer otherwise (the `to_string` + parse path's plain spelling answers the same).
        let zeroes = usize::try_from(-scale).map_err(|_| NumericError::ExponentOverflow)?;
        if let Some(pow) = 10u64
            .checked_pow(u32::try_from(zeroes).map_err(|_| NumericError::ExponentOverflow)?)
            .and_then(|p| i64::try_from(p).ok())
            && let Some(magnitude) = accumulated.checked_mul(pow)
        {
            let magnitude = if negative { -magnitude } else { magnitude };
            return Ok(Self::integer(Integer::from_i64(magnitude)));
        }
        let mut spelling = String::new();
        spelling
            .try_reserve_exact(digits.len() + zeroes + usize::from(negative))
            .map_err(|_| NumericError::Allocation)?;
        if negative {
            spelling.push('-');
        }
        spelling.push_str(digits);
        spelling.extend(core::iter::repeat_n('0', zeroes));
        Self::try_integer_unaccounted(Integer::from_canonical(spelling)?)
    }

    /// Parses a JSON number-literal spelling into a retained-spelling owned number.
    ///
    /// An integer spelling (no fraction and no exponent) keeps its verbatim canonical digits through [`Integer::parse`]
    /// (redundant leading zeroes dropped) — except a NEGATIVE ZERO, whose sign is part of the literal and survives,
    /// exactly as it survives a document decode. `-0` and `-0.0` and `-0e0` are three spellings of one value and must
    /// not answer under three different laws. A decimal spelling (with a fraction or exponent) retains its source
    /// trailing zeroes and scale, so it renders byte-exact through [`crate::NumberView::Decimal`]: `1.000` → `1.000`,
    /// `100E-2` → `1.00`, `1.50` → `1.50`, `0e5` → `0E+5`, `1e400` → `1E+400`. Acceptance is owned by
    /// `decimal::retained_render_parts` (the same grammar and error classes as [`Decimal::parse`]); when the retained
    /// render scale overflows `i64` the value falls back to the canonical decimal (byte behavior degrades to the
    /// round-tripping form, never rejecting an accepted number).
    pub fn try_json_literal(spelling: &str) -> Result<Self, NumericError> {
        if !spelling.contains(['.', 'e', 'E']) {
            let integer = Integer::parse(spelling)?;
            // `parse` normalizes the magnitude, which erases a negative zero's sign; the sign is data here, so the
            // accepted spelling is re-adopted canonically instead.
            if integer.as_str() == "0" && spelling.starts_with('-') {
                return Self::try_integer_unaccounted(Integer::from_canonical_ref("-0")?);
            }
            return Self::try_integer_unaccounted(integer);
        }
        // `retained_render_parts` owns acceptance and the retained build in one pass; the canonical decimal is built
        // only when the retained scale overflows `i64`.
        match decimal::retained_render_parts(spelling)? {
            Some((coefficient, scale)) => {
                let integer = Integer::from_canonical(coefficient)?;
                Self::try_decimal_unaccounted(Decimal::from_literal_parts(integer, scale)?)
            }
            None => Self::try_decimal_unaccounted(Decimal::parse(spelling)?),
        }
    }

    // --- Reads ---

    /// The inline machine integer, when this number is one.
    #[must_use]
    pub fn as_machine(&self) -> Option<i64> {
        match &self.0 {
            NumberStorage::Machine(value) => Some(*value),
            NumberStorage::Float(_) | NumberStorage::Boxed(_) => None,
        }
    }

    /// The owned integer value, rendering the inline arm's spelling on demand and cloning a boxed one.
    ///
    /// An inline machine integer renders into a stack [`Integer`] with no allocation (`Integer::from_i64`); a boxed
    /// integer clones its retained representation — the rare arm (literals, big integers).
    #[must_use]
    pub fn to_integer(&self) -> Option<Integer> {
        match &self.0 {
            NumberStorage::Machine(value) => Some(Integer::from_i64(*value)),
            NumberStorage::Float(_) => None,
            NumberStorage::Boxed(repr) => match repr.as_ref() {
                NumberRepr::Integer(integer) => Some(integer.clone()),
                NumberRepr::Decimal(_) | NumberRepr::Float(_) => None,
            },
        }
    }

    /// `i64` for an integer that fits. `None` for a big integer or a non-integer.
    #[must_use]
    pub fn to_i64(&self) -> Option<i64> {
        match &self.0 {
            NumberStorage::Machine(value) => Some(*value),
            NumberStorage::Float(_) => None,
            NumberStorage::Boxed(repr) => match repr.as_ref() {
                NumberRepr::Integer(integer) => integer.to_i64(),
                NumberRepr::Decimal(_) | NumberRepr::Float(_) => None,
            },
        }
    }

    /// Integer, decimal, or float.
    #[must_use]
    pub fn category(&self) -> NumberCategory {
        match &self.0 {
            NumberStorage::Machine(_) => NumberCategory::Integer,
            NumberStorage::Float(_) => NumberCategory::Float,
            NumberStorage::Boxed(repr) => match repr.as_ref() {
                NumberRepr::Integer(_) => NumberCategory::Integer,
                NumberRepr::Decimal(_) => NumberCategory::Decimal,
                NumberRepr::Float(_) => NumberCategory::Float,
            },
        }
    }

    /// Borrows the boxed integer payload.
    ///
    /// The inline machine arm answers [`None`] even though [`Self::category`] calls that same value `Integer` —
    /// machine integers have no boxed payload to borrow. Guard with [`as_machine`](Self::as_machine) or use
    /// [`to_integer`](Self::to_integer) when either arm must resolve.
    #[must_use]
    pub fn as_integer(&self) -> Option<&Integer> {
        match &self.0 {
            NumberStorage::Machine(_) | NumberStorage::Float(_) => None,
            NumberStorage::Boxed(repr) => match repr.as_ref() {
                NumberRepr::Integer(value) => Some(value),
                NumberRepr::Decimal(_) | NumberRepr::Float(_) => None,
            },
        }
    }

    /// Returns this number as a decimal when its logical category is decimal.
    #[must_use]
    pub fn as_decimal(&self) -> Option<&Decimal> {
        match &self.0 {
            NumberStorage::Boxed(repr) => match repr.as_ref() {
                NumberRepr::Decimal(value) => Some(value),
                NumberRepr::Integer(_) | NumberRepr::Float(_) => None,
            },
            NumberStorage::Machine(_) | NumberStorage::Float(_) => None,
        }
    }

    /// Returns this number as binary64 when its logical category is float.
    #[must_use]
    pub fn as_float(&self) -> Option<Float> {
        match &self.0 {
            NumberStorage::Float(value) => Some(*value),
            NumberStorage::Boxed(repr) => match repr.as_ref() {
                NumberRepr::Float(value) => Some(*value),
                NumberRepr::Integer(_) | NumberRepr::Decimal(_) => None,
            },
            NumberStorage::Machine(_) => None,
        }
    }

    // --- Writes ---

    /// Overwrites a uniquely held boxed representation in place, or moves this handle onto a fresh allocation when the
    /// representation is shared or the handle is inline — the detach-on-write law, stated fully on
    /// [`Self::set_integer`].
    fn set_boxed(&mut self, repr: NumberRepr) {
        match &mut self.0 {
            NumberStorage::Boxed(representation) => {
                if let Some(unique) = Arc::get_mut(representation) {
                    *unique = repr;
                    return;
                }
            }
            NumberStorage::Machine(_) | NumberStorage::Float(_) => {}
        }
        *self = Self::boxed(Arc::new(repr));
    }

    /// Replaces this number's value with `value`, REUSING this handle's representation allocation when it is the only
    /// handle on it.
    ///
    /// This is the detach-on-write law jqf-data's shared payloads already follow, applied to the number representation:
    /// a uniquely-held representation is overwritten in place, a shared one is left untouched while this handle moves
    /// onto a fresh allocation, and an incoming machine value retires any box for the inline arm. Which of the three
    /// happened is not observable — the resulting number is the same either way, and no consumer API can tell two
    /// representation allocations apart.
    ///
    /// The caller it exists for is a fold of `+ - *` over machine integers, where the left operand IS the accumulator
    /// and is dropped the instant the result is built: writing the result into it retires one allocation and one free
    /// per step of the fold.
    pub fn set_integer(&mut self, value: Integer) {
        if let Some(machine) = value.as_machine() {
            self.0 = NumberStorage::Machine(machine);
        } else {
            self.set_boxed(NumberRepr::Integer(value));
        }
    }

    /// Replaces this number's value with a canonical `Decimal`, under the same detach-on-write law as
    /// [`Self::set_integer`] — the decimal fold's twin of the integer arm's allocation retirement.
    pub fn set_decimal(&mut self, value: Decimal) {
        self.set_boxed(NumberRepr::Decimal(value));
    }

    // --- Allocation identity ---

    /// Whether these two numbers hold the SAME representation allocation.
    ///
    /// This is the number half of [`crate::Value::shares_allocation_with`]: two handles on one representation are the
    /// same number OBJECT — a navigation- extracted number stays the register's number while an equal constant or
    /// arithmetic result is a different object and must be rejected.
    #[must_use]
    pub(crate) fn shares_allocation_with(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (NumberStorage::Boxed(a), NumberStorage::Boxed(b)) => Arc::ptr_eq(a, b),
            // Two inline machine integers are two independent objects — they share no allocation, exactly as two
            // independently boxed ones would not.
            _ => false,
        }
    }

    // --- Transforms ---

    /// Whether this number carries a negative sign in its representation.
    ///
    /// A canonical zero (`0`) and a positive value are not negative; a retained negative zero (`-0`, `-0.0`, `-0E5`)
    /// and a negative float (including `-0.0`) are. This is the sign predicate `length`'s sign-strip law consults to
    /// decide whether it can pass a located number's handle through unchanged.
    #[must_use]
    pub fn is_negative(&self) -> bool {
        match &self.0 {
            NumberStorage::Machine(value) => *value < 0,
            NumberStorage::Float(value) => value.get().is_sign_negative(),
            NumberStorage::Boxed(repr) => repr.is_negative(),
        }
    }

    /// Returns this number with its sign stripped, preserving its category, coefficient, and scale (the `length`
    /// sign-strip law, not a general abs).
    ///
    /// The result is an owned number equal in magnitude with any leading minus dropped: an integer keeps its exact
    /// digits (`-10…0` → `10…0`), a decimal keeps its retained coefficient and scale so trailing zeroes and a
    /// zero exponent survive (`-1.000` → `1.000`, `-0.0` → `0.0`, `-0E5` → `0E+5`), and a float returns its
    /// magnitude. Non-negative inputs return an equal value; callers that want zero-cost pass-through should test
    /// [`Self::is_negative`] first.
    ///
    /// # Errors
    ///
    /// Returns [`NumericError`] when allocating the stripped canonical storage fails or the retained coefficient/scale
    /// is invalid.
    pub fn try_sign_stripped(&self) -> Result<Self, NumericError> {
        match &self.0 {
            // The magnitude fits i64 for every value but i64::MIN, whose stripped magnitude is one past i64::MAX and
            // falls out of the machine arm through Integer::parse, exactly as a boxed integer's would.
            NumberStorage::Machine(value) => {
                if let Some(magnitude) = value.checked_abs() {
                    Ok(Self(NumberStorage::Machine(magnitude)))
                } else {
                    let integer = Integer::from_i64(*value);
                    Self::try_integer_unaccounted(Integer::parse(strip_sign(integer.as_str()))?)
                }
            }
            NumberStorage::Float(value) => Ok(Self(NumberStorage::Float(Float::new(value.get().abs())))),
            NumberStorage::Boxed(repr) => repr.try_sign_stripped(),
        }
    }

    /// Returns this number with its sign FLIPPED, preserving its category, coefficient, and scale — the unary-minus
    /// law, in which a zero of either sign negates to a POSITIVE zero.
    ///
    /// Negation flips the stored value's sign, so the spelling survives (`1.500` → `-1.500`, `1E+1000` →
    /// `-1E+1000`, `1.5E+10` → `-1.5E+10`) — which is why this is NOT a subtraction from zero: binary `-` runs in
    /// double arithmetic and answers `-1.5` for `0 - 1.500`. The zero rule is the subtraction sign rule surfacing
    /// through negation: `0.0` → `0.0`, `-0.0` → `0.0`, `0E+5` → `0E+5`, `0.00` → `0.00`, and `0` → `0`, all
    /// POSITIVE.
    ///
    /// A NEGATIVE input is [`Self::try_sign_stripped`] exactly; the two laws differ only for a non-negative one, where
    /// this adds the sign that the strip would leave alone.
    ///
    /// # Errors
    ///
    /// Returns [`NumericError`] when allocating the signed canonical storage fails or the retained coefficient/scale is
    /// invalid.
    pub fn try_negated(&self) -> Result<Self, NumericError> {
        if self.is_negative() {
            return self.try_sign_stripped();
        }
        match &self.0 {
            // The `is_negative` guard above routed every negative machine value (i64::MIN included) to
            // try_sign_stripped, so *value is non-negative here and its negation always fits i64.
            NumberStorage::Machine(value) => Ok(Self(NumberStorage::Machine(-*value))),
            NumberStorage::Float(value) => {
                // A float zero negates to a POSITIVE zero (the zero rule: `-0.0` -> `0.0`), so the zero guard mirrors
                // the boxed arm's.
                if value.get() == 0.0 {
                    return Ok(self.clone());
                }
                Ok(Self(NumberStorage::Float(Float::new(-value.get()))))
            }
            NumberStorage::Boxed(repr) => {
                // A retained zero keeps the owner so its positive spelling survives (the zero rule: `0.0` negates to
                // `0.0`).
                if repr.is_zero() {
                    return Ok(self.clone());
                }
                repr.try_negated()
            }
        }
    }

    #[cfg(test)]
    fn decimal_parts(&self) -> Option<(&str, i64)> {
        self.as_decimal()
            .map(|decimal| (decimal.coefficient().as_str(), decimal.scale()))
    }
}

// --------------------------------------------------------------------------- Text helpers
// ---------------------------------------------------------------------------

/// Drops a single leading minus sign from signed decimal text.
fn strip_sign(text: &str) -> &str {
    text.strip_prefix('-').unwrap_or(text)
}

/// `-` followed by `digits`, in a fallibly-allocated buffer.
pub(crate) fn prefix_minus(digits: &str) -> Result<String, NumericError> {
    let mut signed = String::new();
    signed
        .try_reserve_exact(digits.len() + 1)
        .map_err(|_| NumericError::Allocation)?;
    signed.push('-');
    signed.push_str(digits);
    Ok(signed)
}

#[cfg(test)]
mod tests {
    use super::{Number, NumberCategory};
    use alloc::string::{String, ToString};

    /// Shortest-round-trip constructor matches `to_string` + parse: same value, category, and retained spelling.
    #[test]
    fn shortest_f64_matches_the_string_round_trip() {
        let values = [
            1.5,
            0.5,
            -0.75,
            1.0 / 3.0,
            core::f64::consts::E,
            core::f64::consts::PI,
            1e21,
            1e-21,
            123.456,
            1e-17,
            1e100,
            5e-324,
            -1e308,
            7.0 / 9.0,
            0.000_976_562_5,
            123_456_789.123_456_78,
            -0.0,
            0.0,
            -1.0,
            1e16,
        ];
        for value in values {
            let direct = Number::try_from_shortest_f64(value).expect("direct");
            let round_trip = Number::try_json_literal(&value.to_string()).expect("round trip");
            assert_eq!(direct.category(), round_trip.category(), "category {value:?}");
            match (direct.as_decimal(), round_trip.as_decimal()) {
                (Some(a), Some(b)) => assert_eq!(
                    (a.coefficient().as_str(), a.scale()),
                    (b.coefficient().as_str(), b.scale()),
                    "decimal {value:?}"
                ),
                _ => assert_eq!(
                    direct.to_integer().expect("integer").as_str(),
                    round_trip.to_integer().expect("integer").as_str(),
                    "integer {value:?}"
                ),
            }
        }
    }

    fn decimal(spelling: &str) -> (String, i64) {
        let number = Number::try_json_literal(spelling).expect("literal parses");
        assert_eq!(number.category(), NumberCategory::Decimal, "{spelling}");
        let (coefficient, scale) = number.decimal_parts().expect("decimal parts");
        (String::from(coefficient), scale)
    }

    fn integer(spelling: &str) -> String {
        let number = Number::try_json_literal(spelling).expect("literal parses");
        assert_eq!(number.category(), NumberCategory::Integer, "{spelling}");
        String::from(number.to_integer().expect("integer").as_str())
    }

    /// The integer arm's canonicalization: redundant leading zeroes go, the digits and the sign stay.
    ///
    /// The `-0` family is the rows that bite. `-0` is the one spelling whose sign carries no magnitude, so a
    /// normalizing parse erases it — and then `-0`, `-0.0` and `-0e0` would be three spellings of one value answering
    /// under three different laws, and this acceptor would disagree with the document decode of the same bytes. `-000`
    /// pins that the retained sign survives the leading-zero strip that produced it.
    #[test]
    fn integer_literals_keep_canonical_digits() {
        assert_eq!(integer("10"), "10");
        assert_eq!(integer("0"), "0");
        assert_eq!(integer("100"), "100");
        assert_eq!(integer("12300"), "12300");
        assert_eq!(integer("-5"), "-5");
        assert_eq!(integer("-0"), "-0");
        assert_eq!(integer("-000"), "-0");
        assert_eq!(integer("+0"), "0");
        assert_eq!(integer("10000000000000000000"), "10000000000000000000");
        // The sibling spellings of the same value, under the decimal arm's retained-spelling law: all three keep the
        // sign.
        assert_eq!(decimal("-0.0"), (String::from("-0"), 1));
        assert_eq!(decimal("-0e0"), (String::from("-0"), 0));
    }

    /// `try_json_literal` accepts every leading-zero decimal spelling `Decimal::parse` accepts: the retained
    /// coefficient drops the mantissa's leading zeros, so a leading-zero spelling retains the same parts as its
    /// stripped twin — including when the first nonzero digit is not the first character of the mantissa. Interior
    /// zeros survive the strip and the sign survives.
    #[test]
    fn decimal_literals_accept_leading_zero_mantissas() {
        assert_eq!(decimal("0123e1"), decimal("123e1"));
        assert_eq!(decimal("0.0012e1"), (String::from("12"), 3));
        assert_eq!(decimal("01020e1"), (String::from("1020"), -1));
        assert_eq!(decimal("-0123e1"), (String::from("-123"), -1));
    }

    /// Derived `Decimal` equality is by `(coefficient, scale)`, not by value. `"1.0"` and `"1.00"` compare unequal;
    /// their parsed forms compare equal.
    #[test]
    fn decimal_equality_is_spelling_based_but_parse_normalizes() {
        let a = Number::try_json_literal("1.0")
            .expect("literal")
            .as_decimal()
            .expect("decimal")
            .clone();
        let b = Number::try_json_literal("1.00")
            .expect("literal")
            .as_decimal()
            .expect("decimal")
            .clone();
        // Retained spellings differ: (10, 1) vs (100, 2), both the value 1.
        assert_eq!(a.coefficient().as_str(), "10");
        assert_eq!(b.coefficient().as_str(), "100");
        assert_ne!(a, b, "derived Decimal eq is spelling-based");
        // The canonical (parsed) forms normalize to the same parts — the value comparison a consumer actually
        // performs.
        let canonical_a = super::Decimal::parse("1.0").expect("parse");
        let canonical_b = super::Decimal::parse("1.00").expect("parse");
        assert_eq!(canonical_a, canonical_b);
    }

    /// These are the RETAINED parts. The canonical parse of the same spellings trims the trailing zeroes (decimal.rs
    /// pins that law separately): `1.50` retains `(150, 2)` below yet parses to `(15, 1)`.
    #[test]
    fn decimal_literals_retain_spelling_as_coefficient_and_scale() {
        // Each pair is what `write_decimal_scientific(coefficient, scale)` renders byte-exact.
        assert_eq!(decimal("1.000"), (String::from("1000"), 3)); // -> 1.000
        assert_eq!(decimal("100E-2"), (String::from("100"), 2)); // -> 1.00
        assert_eq!(decimal("1.50"), (String::from("150"), 2)); // -> 1.50
        assert_eq!(decimal("3.5"), (String::from("35"), 1)); // -> 3.5
        assert_eq!(decimal("1.0"), (String::from("10"), 1)); // -> 1.0
        assert_eq!(decimal("0.5"), (String::from("5"), 1)); // -> 0.5
        assert_eq!(decimal("0.001"), (String::from("1"), 3)); // -> 0.001
        assert_eq!(decimal("1230e-1"), (String::from("1230"), 1)); // -> 123.0
        assert_eq!(decimal("1e2"), (String::from("1"), -2)); // -> 1E+2
        assert_eq!(decimal("0e5"), (String::from("0"), -5)); // -> 0E+5
        assert_eq!(decimal("0.0"), (String::from("0"), 1)); // -> 0.0
        assert_eq!(decimal("1e400"), (String::from("1"), -400)); // -> 1E+400
        assert_eq!(decimal("-1.50"), (String::from("-150"), 2)); // -> -1.50
    }

    #[test]
    fn sign_strip_preserves_class_and_magnitude() {
        // Negatives lose the sign and keep coefficient and scale. Owned integer `-0` parses as `0`, so it is not
        // negative here.
        let cases = [
            ("-5", "5", None),
            ("-10000000000000000000", "10000000000000000000", None),
            ("-1.000", "1000", Some(3)),
            ("-0.0", "0", Some(1)),
            ("-0E5", "0", Some(-5)),
            ("-3.5", "35", Some(1)),
        ];
        for (spelling, coefficient, scale) in cases {
            let number = Number::try_json_literal(spelling).expect("literal");
            assert!(number.is_negative(), "{spelling} must be negative");
            let stripped = number.try_sign_stripped().expect("strip");
            assert!(!stripped.is_negative(), "{spelling} strips to non-negative");
            match scale {
                Some(scale) => {
                    let (got_coeff, got_scale) = stripped.decimal_parts().expect("decimal");
                    assert_eq!((got_coeff, got_scale), (coefficient, scale), "{spelling}");
                }
                None => {
                    assert_eq!(
                        stripped.to_integer().expect("integer").as_str(),
                        coefficient,
                        "{spelling}"
                    );
                }
            }
        }
    }

    /// The sign-strip law is BLIND to the storage arm: `is_negative` and `try_sign_stripped` must answer the same for a
    /// machine-arm integer and a heap-arm one with the same bytes.
    ///
    /// Both read the integer through the borrowed retained spelling, so the arm must be invisible to them. The rows
    /// walk the ±10^k boundaries, 2^53±1, the `i64` extremes and one step past each: `i64::MIN` is the row that
    /// matters most, because its magnitude has no `i64` counterpart and the stripped value must leave the machine arm
    /// for the heap one.
    #[test]
    fn sign_strip_is_blind_to_the_integer_storage_arm() {
        let cases = [
            ("0", false, "0"),
            ("1", false, "1"),
            ("-1", true, "1"),
            ("-10", true, "10"),
            ("-1000", true, "1000"),
            ("9007199254740993", false, "9007199254740993"),
            ("-9007199254740993", true, "9007199254740993"),
            ("9223372036854775807", false, "9223372036854775807"),
            // `i64::MIN` strips to a magnitude one PAST `i64::MAX`, so the machine arm cannot hold the result and the
            // heap arm must.
            ("-9223372036854775808", true, "9223372036854775808"),
            ("-9223372036854775809", true, "9223372036854775809"),
            ("-99999999999999999999", true, "99999999999999999999"),
            ("-12345678901234567890123456789", true, "12345678901234567890123456789"),
        ];
        for (spelling, negative, stripped) in cases {
            let number = Number::try_json_literal(spelling).expect("literal");
            assert_eq!(number.category(), NumberCategory::Integer, "{spelling}");
            assert_eq!(number.is_negative(), negative, "is_negative {spelling}");
            let result = number.try_sign_stripped().expect("strip");
            assert_eq!(result.category(), NumberCategory::Integer, "{spelling}");
            assert_eq!(
                result.to_integer().expect("integer").as_str(),
                stripped,
                "strip {spelling}"
            );
            assert!(!result.is_negative(), "stripped {spelling} is non-negative");
        }
    }

    /// The retained `-0` an integer-category number can carry from a document: negative by the sign predicate, positive
    /// zero once stripped, and equal in VALUE to the machine zero it does not share bytes with.
    #[test]
    fn a_retained_negative_zero_integer_is_negative_and_strips_to_zero() {
        let negative_zero = Number::integer(super::Integer::from_canonical(String::from("-0")).expect("canonical"));
        assert!(negative_zero.is_negative());
        assert_eq!(negative_zero.to_integer().expect("integer").as_str(), "-0");
        assert_eq!(negative_zero.to_integer().expect("integer").to_i64(), Some(0));
        let stripped = negative_zero.try_sign_stripped().expect("strip");
        assert!(!stripped.is_negative());
        assert_eq!(stripped.to_integer().expect("integer").as_str(), "0");
    }

    /// `set_integer` reuses a UNIQUELY held representation and detaches a shared one, and the resulting VALUE is the
    /// same either way.
    ///
    /// The sharing back door is the only observer of the difference — which is the property being asserted: the two
    /// arms differ in which allocation holds the answer and in nothing else.
    #[test]
    fn setting_an_integer_detaches_a_shared_representation() {
        // A BIG integer starts boxed, so a clone shares the representation — the machine arm is inline and has
        // nothing to share, which is the point of it.
        let mut unique = Number::try_json_literal("123456789012345678901234567890").expect("literal");
        let witness = unique.clone();
        let same_allocation = unique.shares_allocation_with(&witness);
        drop(witness);
        assert!(same_allocation, "a clone shares the representation");

        // Nothing else holds it now, so the write lands in place.
        unique.set_integer(super::Integer::from_i64(42));
        assert_eq!(unique.to_integer().expect("integer").as_str(), "42");

        let mut shared = Number::try_json_literal("123456789012345678901234567890").expect("literal");
        let other = shared.clone();
        shared.set_integer(super::Integer::from_i64(42));
        assert_eq!(shared.to_integer().expect("integer").as_str(), "42");
        assert_eq!(
            other.to_integer().expect("integer").as_str(),
            "123456789012345678901234567890",
            "a shared representation is never written through"
        );
        assert!(!shared.shares_allocation_with(&other));

        // The category can change with the value: an integer written over a decimal leaves nothing of the decimal
        // behind.
        let mut was_decimal = Number::try_json_literal("1.50").expect("literal");
        was_decimal.set_integer(super::Integer::from_i64(-7));
        assert_eq!(was_decimal.category(), NumberCategory::Integer);
        assert_eq!(was_decimal.to_integer().expect("integer").as_str(), "-7");
        assert!(was_decimal.as_decimal().is_none());
    }

    /// `set_decimal` holds the same detach-on-write law as `set_integer`: a uniquely-held representation is overwritten
    /// in place, a shared one is left untouched while this handle moves onto a fresh allocation, and the machine arm
    /// has no representation to reuse so the write moves the number onto a boxed decimal.
    #[test]
    fn setting_a_decimal_detaches_a_shared_representation() {
        let fifteen = || super::Decimal::from_parts(super::Integer::from_i64(15), 1).expect("decimal");

        let mut unique = Number::try_json_literal("123456789012345678901234567890").expect("literal");
        let witness = unique.clone();
        let same_allocation = unique.shares_allocation_with(&witness);
        drop(witness);
        assert!(same_allocation, "a clone shares the representation");
        unique.set_decimal(fifteen());
        assert_eq!(unique.decimal_parts(), Some(("15", 1)));

        let mut shared = Number::try_json_literal("123456789012345678901234567890").expect("literal");
        let other = shared.clone();
        shared.set_decimal(fifteen());
        assert_eq!(shared.decimal_parts(), Some(("15", 1)));
        assert_eq!(
            other.to_integer().expect("integer").as_str(),
            "123456789012345678901234567890",
            "a shared representation is never written through"
        );
        assert!(!shared.shares_allocation_with(&other));

        let mut was_machine = Number::integer(super::Integer::from_i64(3));
        assert!(was_machine.as_machine().is_some(), "inline before the write");
        was_machine.set_decimal(super::Decimal::from_parts(super::Integer::from_i64(9), 2).expect("decimal"));
        assert_eq!(was_machine.decimal_parts(), Some(("9", 2)));
    }

    /// The retained-scale overflow fallback: `1000e-9223372036854775808` keeps a coefficient of `1000`, whose retained
    /// scale is 2^63 and cannot be stored, so `try_json_literal` falls back to the canonical parse — which trims the
    /// trailing zeroes into a storable scale. The value is the same either way; only the retained spelling is lost. A
    /// spelling whose CANONICAL scale does not fit either rejects with the canonical parse's class — the fallback
    /// never widens acceptance.
    #[test]
    fn json_literal_falls_back_to_canonical_when_the_retained_scale_overflows() {
        let number = Number::try_json_literal("1000e-9223372036854775808").expect("literal");
        assert_eq!(number.category(), NumberCategory::Decimal);
        assert_eq!(
            number.decimal_parts(),
            Some(("1", 9_223_372_036_854_775_805)),
            "trailing zeroes trim the retained scale into range"
        );
        assert!(
            matches!(
                Number::try_json_literal("1e9223372036854775809"),
                Err(super::NumericError::ExponentOverflow)
            ),
            "the fallback never widens acceptance"
        );
    }

    /// The non-negative arm of the sign-strip law: a value with no sign to strip comes back EQUAL — same category,
    /// same digits or the same retained coefficient and scale — never merely same-shaped.
    #[test]
    fn sign_strip_leaves_non_negative_values_equal() {
        for spelling in ["0", "5", "1.000", "0E5"] {
            let number = Number::try_json_literal(spelling).expect("literal");
            assert!(!number.is_negative(), "{spelling}");
            let stripped = number.try_sign_stripped().expect("strip");
            assert_eq!(stripped.category(), number.category(), "{spelling}");
            match number.decimal_parts() {
                Some((coefficient, scale)) => assert_eq!(
                    stripped.decimal_parts(),
                    Some((coefficient, scale)),
                    "coefficient and scale survive {spelling}"
                ),
                None => assert_eq!(
                    stripped.to_integer().expect("integer").as_str(),
                    number.to_integer().expect("integer").as_str(),
                    "digits survive {spelling}"
                ),
            }
        }
    }

    /// The unary-minus law over the EXACT categories: an integer's digits and a decimal's retained coefficient/scale
    /// survive the flip, so the spelling a document carried is the spelling the negation renders (`1.500` → `-1.500`,
    /// `1E+1000` → `-1E+1000`).
    #[test]
    fn negation_flips_the_sign_and_keeps_the_retained_spelling() {
        for (spelling, negated_digits) in [
            ("5", "-5"),
            ("-5", "5"),
            ("13911860366432393", "-13911860366432393"),
            ("-13911860366432393", "13911860366432393"),
        ] {
            let number = Number::try_json_literal(spelling).expect("literal");
            let negated = number.try_negated().expect("negate");
            assert_eq!(negated.category(), NumberCategory::Integer, "{spelling}");
            assert_eq!(
                negated.to_integer().expect("integer").as_str(),
                negated_digits,
                "negate {spelling}"
            );
        }
        for (spelling, coefficient, scale) in [
            ("1.500", "-1500", 3),
            ("-1.500", "1500", 3),
            ("100.0", "-1000", 1),
            ("1E+1000", "-1", -1000),
            ("1.5E+10", "-15", -9),
            ("1E-1000", "-1", 1000),
            ("0.12345678901234567890123456789", "-12345678901234567890123456789", 29),
        ] {
            let number = Number::try_json_literal(spelling).expect("literal");
            let negated = number.try_negated().expect("negate");
            assert_eq!(negated.category(), NumberCategory::Decimal, "{spelling}");
            assert_eq!(negated.decimal_parts(), Some((coefficient, scale)), "negate {spelling}");
        }
    }

    /// …and every zero, of either sign and in either exact category, negates to a POSITIVE zero while keeping its
    /// scale: `-0.0` answers `0.0` and `-0e5` answers `0E+5` — the subtraction sign rule surfacing through negation.
    #[test]
    fn negation_keeps_every_zero_positive() {
        for spelling in ["0", "-0"] {
            let negated = Number::try_json_literal(spelling)
                .expect("literal")
                .try_negated()
                .expect("negate");
            assert!(!negated.is_negative(), "{spelling}");
            assert_eq!(negated.to_integer().expect("integer").as_str(), "0");
        }
        for (spelling, scale) in [("0.0", 1), ("-0.0", 1), ("0.00", 2), ("0e5", -5)] {
            let negated = Number::try_json_literal(spelling)
                .expect("literal")
                .try_negated()
                .expect("negate");
            assert!(!negated.is_negative(), "{spelling}");
            assert_eq!(negated.decimal_parts(), Some(("0", scale)), "{spelling}");
        }
    }

    /// A computed FLOAT negates by IEEE sign flip, and its zero stays positive like every other zero.
    ///
    /// The bit comparisons are exact on purpose: this is a SIGN law, and `0.0 == -0.0` under IEEE equality, so only the
    /// bits distinguish the answer this test exists to pin.
    #[test]
    fn a_float_negates_by_sign_flip_and_its_zero_stays_positive() {
        let float = Number::float(super::Float::new(1.5));
        let negated = float.try_negated().expect("negate");
        assert_eq!(negated.category(), NumberCategory::Float);
        assert_eq!(negated.as_float().expect("float").get().to_bits(), (-1.5_f64).to_bits());

        for zero in [0.0_f64, -0.0_f64] {
            let negated = Number::float(super::Float::new(zero)).try_negated().expect("negate");
            let value = negated.as_float().expect("float").get();
            assert_eq!(
                value.to_bits(),
                0.0_f64.to_bits(),
                "a float zero negates to POSITIVE zero"
            );
        }
    }
}

#[cfg(test)]
mod size_laws {
    use super::Number;
    use crate::Value;

    /// Pins the sizes before the representation work, so the storage enum cannot silently grow `Value` — the box's
    /// original reason. Its three arms (inline machine integer, inline float, one boxed allocation) cost the struct 16
    /// bytes, and `Value` keeps its 32: the naive enum fits the niche, no pointer tagging.
    #[test]
    fn value_size_is_pinned_at_32_with_the_inline_and_boxed_number_arms() {
        assert_eq!(core::mem::size_of::<Number>(), 16);
        assert_eq!(core::mem::size_of::<Value>(), 32);
        assert_eq!(core::mem::size_of::<super::Integer>(), 40);
    }
}
