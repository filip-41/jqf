//! Exact finite base-ten decimals: `coefficient * 10^-scale`.
//!
//! [`Decimal::parse`] and [`Decimal::from_parts_normalizing`] drop trailing zeroes. [`Decimal::from_literal_parts`]
//! keeps the source spelling. Derived `Eq` / `Hash` / `Ord` compare `(coefficient, scale)`, not the mathematical value.
//! Rendering lives in `text.rs`.

use alloc::string::String;

use super::{Integer, NumericError};

/// Exact decimal: `coefficient * 10^-scale`.
///
/// Derived `Eq`, `Hash`, and `Ord` compare `(coefficient, scale)`, not the mathematical value. `"1.0"` (`10` at scale
/// `1`) and `"1.00"` (`100` at scale `2`) are unequal here. Parse and [`Decimal::from_parts_normalizing`] normalize. Do
/// not use derived equality as a value test.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Decimal {
    coefficient: Integer,
    scale: i64,
}

impl Decimal {
    /// Validates already-normalized coefficient/scale parts without allocating or normalizing them.
    pub(crate) fn validate_canonical_parts(coefficient: &str, scale: i64) -> Result<(), NumericError> {
        Integer::validate_canonical(coefficient)?;
        let digits = coefficient.trim_start_matches('-');
        if digits == "0" && scale != 0 {
            return Err(NumericError::InvalidDigit);
        }
        if digits.len() > 1 && digits.ends_with('0') {
            return Err(NumericError::InvalidDigit);
        }
        Ok(())
    }

    /// Parses a finite decimal spelling and normalizes its coefficient/scale.
    ///
    /// The constructor accepts a decimal point and/or exponent. Codec lexical rules must be validated before calling
    /// it.
    pub fn parse(spelling: &str) -> Result<Self, NumericError> {
        let (mantissa, exponent) = split_exponent(spelling)?;
        let (negative, unsigned, point) = split_mantissa(mantissa)?;
        let fractional_digits = unsigned.len() - point - usize::from(point < unsigned.len());
        let fractional_digits = i128::try_from(fractional_digits).map_err(|_| NumericError::ExponentOverflow)?;
        let exponent = exponent
            .map_or(Ok(0_i128), str::parse::<i128>)
            .map_err(|_| NumericError::InvalidExponent)?;
        let mut scale = fractional_digits
            .checked_sub(exponent)
            .ok_or(NumericError::ExponentOverflow)?;

        // Machine fast path: accumulate the digits into `i64` with no intermediate String, so a canonical machine
        // decimal pays one allocation (the boxed Decimal) instead of a scratch String first. A zero between nonzeros is
        // a VALUE digit (`10.5` → 105), so pending trailing zeros are shifted back in when a nonzero follows; only
        // zeros after the last nonzero are the canonical trailing-zero trim. Any overflow falls through to the
        // arbitrary-precision arm below, which answers identically.
        let mut acc: i64 = 0;
        let mut trailing_zeroes: u32 = 0;
        let mut machine_ok = true;
        for byte in unsigned.bytes() {
            if byte == b'.' {
                continue;
            }
            let digit = i64::from(byte - b'0');
            if digit == 0 {
                if acc == 0 {
                    continue; // leading zero: no value yet
                }
                let Some(next) = trailing_zeroes.checked_add(1) else {
                    machine_ok = false;
                    break;
                };
                trailing_zeroes = next;
            } else if acc == 0 {
                acc = digit;
            } else {
                let Some(zeros) = trailing_zeroes.checked_add(1) else {
                    machine_ok = false;
                    break;
                };
                let Some(pow) = 10i64.checked_pow(zeros) else {
                    machine_ok = false;
                    break;
                };
                let Some(next) = acc.checked_mul(pow).and_then(|v| v.checked_add(digit)) else {
                    machine_ok = false;
                    break;
                };
                acc = next;
                trailing_zeroes = 0;
            }
        }
        if machine_ok {
            if acc == 0 {
                return Ok(Self {
                    coefficient: Integer::from_i64(0),
                    scale: 0,
                });
            }
            if trailing_zeroes != 0 {
                scale = scale
                    .checked_sub(i128::from(trailing_zeroes))
                    .ok_or(NumericError::ExponentOverflow)?;
            }
            let coefficient = Integer::from_i64(if negative { -acc } else { acc });
            let scale = i64::try_from(scale).map_err(|_| NumericError::ExponentOverflow)?;
            return Ok(Self { coefficient, scale });
        }

        // Arbitrary-precision fallback: the digits overflow `i64` (or the spelling is absurdly long) and need the
        // big-integer arm.
        let mut digits = String::new();
        digits
            .try_reserve_exact(unsigned.len() + usize::from(negative))
            .map_err(|_| NumericError::Allocation)?;
        if negative {
            digits.push('-');
        }
        digits.extend(unsigned.bytes().filter(|byte| *byte != b'.').map(char::from));
        let mut coefficient = Integer::from_decimal_digits(digits);
        if coefficient.as_str() == "0" {
            return Ok(Self { coefficient, scale: 0 });
        }
        let trailing_zeroes = coefficient.trim_trailing_zeroes();
        if trailing_zeroes != 0 {
            scale = scale
                .checked_sub(i128::try_from(trailing_zeroes).map_err(|_| NumericError::ExponentOverflow)?)
                .ok_or(NumericError::ExponentOverflow)?;
        }
        let scale = i64::try_from(scale).map_err(|_| NumericError::ExponentOverflow)?;
        Ok(Self { coefficient, scale })
    }

    /// Creates an already-normalized decimal.
    ///
    /// The parts must pass [`Self::validate_canonical_parts`]. One admitted spelling is not canonical zero: a `-0`
    /// coefficient at scale `0` passes (the same negative-zero retention law as [`Integer::validate_canonical`]) and
    /// stays distinct from `0` under the derived `Eq`/`Hash`. A caller that needs semantic zero normalizes first.
    pub fn from_parts(coefficient: Integer, scale: i64) -> Result<Self, NumericError> {
        Self::validate_canonical_parts(coefficient.as_str(), scale)?;
        Ok(Self { coefficient, scale })
    }

    /// Creates a retained-spelling (literal) decimal from a possibly non-canonical coefficient and scale.
    ///
    /// Unlike [`Self::from_parts`], the coefficient may keep trailing zeroes (`1000` at scale `3` renders `1.000`) and
    /// a zero magnitude may pair with a nonzero scale (`0` at scale `-5` renders `0E+5`): only the coefficient's digit
    /// grammar is validated ([`Integer::validate_canonical`] — the scale is deliberately unchecked), not the strict
    /// canonical grammar. Such a decimal is meant for byte-exact rendering of a source or program literal through
    /// [`crate::NumberView::Decimal`]. Materialization keeps it verbatim via [`Self::from_literal_validated_parts`]
    /// (`1.000` in, `1.000` out); [`Self::from_parts_normalizing`] exists separately for callers that want the trimmed
    /// form. The digit grammar (sign, no leading zero in a longer magnitude) still holds.
    pub fn from_literal_parts(coefficient: Integer, scale: i64) -> Result<Self, NumericError> {
        Integer::validate_canonical(coefficient.as_str())?;
        Ok(Self { coefficient, scale })
    }

    /// Creates a retained-spelling literal decimal whose coefficient the caller has already validated (the materialize
    /// path's `Integer::from_canonical_ref` just ran the same canonical digit grammar over the same bytes — the
    /// literal form adds no rules beyond it).
    pub(crate) fn from_literal_validated_parts(coefficient: Integer, scale: i64) -> Self {
        Self { coefficient, scale }
    }

    /// Creates a decimal from a possibly non-canonical coefficient and scale, normalizing trailing zeroes and a zero
    /// magnitude to canonical form.
    ///
    /// `value == coefficient * 10^-scale` is preserved: dropping `n` trailing zeroes divides the coefficient by `10^n`,
    /// so the scale drops by `n`. A zero magnitude collapses to the single canonical zero at scale `0`.
    pub fn from_parts_normalizing(mut coefficient: Integer, scale: i64) -> Result<Self, NumericError> {
        let is_zero = {
            let text = coefficient.as_str();
            text.strip_prefix('-').unwrap_or(text) == "0"
        };
        if is_zero {
            return Self::from_parts(Integer::from_i64(0), 0);
        }
        let trailing = coefficient.trim_trailing_zeroes();
        let scale = if trailing == 0 {
            scale
        } else {
            scale
                .checked_sub(i64::try_from(trailing).map_err(|_| NumericError::ExponentOverflow)?)
                .ok_or(NumericError::ExponentOverflow)?
        };
        Self::from_parts(coefficient, scale)
    }

    /// Returns the coefficient in `coefficient * 10^-scale` AS RETAINED.
    ///
    /// It is trailing-zero normalized only when the decimal came through a normalizing constructor; a retained literal
    /// keeps its source spelling, so `"1.00"`'s coefficient is `100`, not `1`. The struct doc names which constructors
    /// normalize.
    #[must_use]
    pub const fn coefficient(&self) -> &Integer {
        &self.coefficient
    }

    /// Returns the power in `coefficient * 10^-scale`.
    #[must_use]
    pub const fn scale(&self) -> i64 {
        self.scale
    }

    /// Nearest binary64. Same rounding as `{coefficient}e{-scale}` parse. A retained `-0` coefficient becomes negative
    /// zero.
    #[must_use]
    pub fn to_f64(&self) -> f64 {
        decimal_parts_to_f64(self.coefficient().as_str(), self.scale)
    }
}

/// `10^t` for `t in 0..=22` — every entry is EXACTLY representable in binary64 (`10^t = 2^t · 5^t`, `5^22 < 2^53`),
/// so the bounded fast path's multiply/divide receives a power that cannot itself round.
const POWERS_OF_TEN: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18, 1e19, 1e20,
    1e21, 1e22,
];

/// Same as [`Decimal::to_f64`], over borrowed parts.
///
/// # Panics
///
/// Only on the impossible case that a 20-byte scratch cannot hold an `i64` exponent's decimal digits; see the writer
/// call below.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "the bounded fast path casts a mantissa bounded below 2^53 and a scale bounded below 23 — both exact, both in range"
)]
pub fn decimal_parts_to_f64(coefficient: &str, scale: i64) -> f64 {
    // Bounded fast path: a machine coefficient (≤ 2^53, ≤ 16 digits) with
    // |scale| ≤ 22 makes BOTH operands exact in binary64, so the single
    // multiply/divide is the correctly rounded value — byte-identical to the str::parse fallback by construction
    // (both round the same real to nearest), with no render buffer and no parse.
    let digits = coefficient.strip_prefix('-').unwrap_or(coefficient);
    if digits.len() <= 16 && scale.unsigned_abs() <= 22 {
        let mut mantissa: u64 = 0;
        let mut digits_only = true;
        for byte in digits.bytes() {
            // Non-digit text is not this path's business — it is the fallback below that parses a coefficient as
            // written, so the accumulator never subtracts `b'0'` from a byte that is not a digit.
            if !byte.is_ascii_digit() {
                digits_only = false;
                break;
            }
            mantissa = mantissa * 10 + u64::from(byte - b'0');
        }
        if digits_only && mantissa <= (1_u64 << 53) {
            // `mantissa as f64` is EXACT below 2^53 (the bound just checked), so the one multiply/divide rounds the
            // true value once — the correctly rounded answer.
            let power = POWERS_OF_TEN[scale.unsigned_abs() as usize];
            let magnitude = if scale > 0 {
                mantissa as f64 / power
            } else {
                mantissa as f64 * power
            };
            return if coefficient.starts_with('-') {
                -magnitude
            } else {
                magnitude
            };
        }
    }
    // `{coefficient}e{-scale}`: `-scale`'s magnitude is at most 2^63, which spells 19 digits, so 128 bytes covers
    // coefficients up to ~106 digits; a longer one falls back to the heap path with the same bytes. Writing the
    // exponent from `scale.unsigned_abs()` is deliberate: `format!` on `-i64::MIN` would wrap (debug: panic), where the
    // true exponent is 2^63.
    let exponent_sign = usize::from(scale > 0);
    // `unsigned_abs` names the magnitude for both signs (`-i64::MIN`'s exponent is 2^63); the digits render through the
    // shared u128-to-decimal writer text.rs owns.
    let exponent_magnitude = u128::from(scale.unsigned_abs());
    let mut exponent_digits = [0_u8; 20];
    // A 20-byte scratch holds every i64 magnitude's digits, so the writer cannot run out here.
    let exponent_len = super::text::write_u128_decimal(&mut exponent_digits, exponent_magnitude)
        .expect("a 20-byte scratch holds every i64 exponent");
    let digits = coefficient.as_bytes();
    let mut buffer = [0_u8; 128];
    let tail = 1 + exponent_sign + exponent_len;
    if digits.len() + tail <= buffer.len() {
        let mut len = digits.len();
        buffer[..len].copy_from_slice(digits);
        buffer[len] = b'e';
        len += 1;
        if exponent_sign == 1 {
            buffer[len] = b'-';
            len += 1;
        }
        buffer[len..len + exponent_len].copy_from_slice(&exponent_digits[..exponent_len]);
        len += exponent_len;
        return core::str::from_utf8(&buffer[..len])
            .ok()
            .and_then(|text| text.parse().ok())
            .unwrap_or(f64::NAN);
    }
    // A coefficient wider than the stack buffer keeps the heap route with the same bytes (and the correct exponent, as
    // above).
    let exponent = if scale > 0 {
        alloc::format!("-{scale}")
    } else {
        alloc::format!("{}", scale.unsigned_abs())
    };
    alloc::format!("{coefficient}e{exponent}").parse().unwrap_or(f64::NAN)
}

/// Splits a spelling at its single `e`/`E` exponent separator, returning the mantissa and the exponent text with its
/// sign, when present.
///
/// The exponent grammar is part of the shared acceptance law: a second separator, or an exponent with no digits, is
/// [`NumericError::InvalidExponent`].
fn split_exponent(spelling: &str) -> Result<(&str, Option<&str>), NumericError> {
    let Some(index) = spelling.find(['e', 'E']) else {
        return Ok((spelling, None));
    };
    if spelling[index + 1..].contains(['e', 'E']) {
        return Err(NumericError::InvalidExponent);
    }
    let exponent = &spelling[index + 1..];
    let digits = match exponent.as_bytes().first() {
        Some(b'+' | b'-') => &exponent[1..],
        Some(_) | None => exponent,
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(NumericError::InvalidExponent);
    }
    Ok((&spelling[..index], Some(exponent)))
}

/// Splits a mantissa into its sign, its unsigned digit run, and the byte index of the single decimal point
/// (`unsigned.len()` when absent).
///
/// This is the digit grammar [`Decimal::parse`] and [`retained_render_parts`] share: an optional leading sign followed
/// by digits plus at most one point, with no digits at all rejected as empty.
fn split_mantissa(mantissa: &str) -> Result<(bool, &str, usize), NumericError> {
    let (negative, unsigned) = match mantissa.as_bytes().first() {
        Some(b'-') => (true, &mantissa[1..]),
        Some(b'+') => (false, &mantissa[1..]),
        Some(_) => (false, mantissa),
        None => return Err(NumericError::Empty),
    };
    let mut point = None;
    for (index, byte) in unsigned.bytes().enumerate() {
        match byte {
            b'.' if point.replace(index).is_none() => {}
            b'0'..=b'9' => {}
            _ => return Err(NumericError::InvalidDigit),
        }
    }
    let point = point.unwrap_or(unsigned.len());
    if unsigned.len() == usize::from(point < unsigned.len()) {
        return Err(NumericError::Empty);
    }
    Ok((negative, unsigned, point))
}

/// Computes the retained-spelling render coefficient and scale of a decimal literal, OWNING acceptance exactly as
/// [`Decimal::parse`] does (the same grammar, the same error classes, in the same order): the coefficient keeps the
/// source trailing zeroes (from the first nonzero digit to the last digit), and the scale is `fraction_digits -
/// exponent`. Returns `Ok(None)` for the two spellings the retained form cannot store — an exponent past `i128` and a
/// retained scale past `i64` — and the caller then falls back to the canonical [`Decimal::parse`], which either
/// succeeds or reports the refusal with its own error class.
pub(super) fn retained_render_parts(spelling: &str) -> Result<Option<(String, i64)>, NumericError> {
    let (mantissa, exponent_text) = split_exponent(spelling)?;
    let (negative, unsigned, point) = split_mantissa(mantissa)?;
    let exponent: i128 = match exponent_text {
        Some(text) => match text.parse() {
            Ok(value) => value,
            // Digit-valid but overflows i128: `Decimal::parse` rejects it, so the caller's canonical fallback errors
            // with the same class.
            Err(_) => return Ok(None),
        },
        None => 0,
    };
    let (int_digits, fraction_digits) = if point == unsigned.len() {
        (unsigned, "")
    } else {
        (&unsigned[..point], &unsigned[point + 1..])
    };
    let fraction_len = i128::try_from(fraction_digits.len()).map_err(|_| NumericError::ExponentOverflow)?;
    let Some(scale) = fraction_len
        .checked_sub(exponent)
        .and_then(|scale| i64::try_from(scale).ok())
    else {
        return Ok(None);
    };
    let combined = int_digits.bytes().chain(fraction_digits.bytes());
    let first_nonzero = combined.clone().position(|byte| byte != b'0');
    let mut coefficient = String::new();
    match first_nonzero {
        // An all-zero magnitude renders from the sign-carrying `0`/`-0`; only the scale distinguishes `0.0` from `0e5`.
        None => {
            coefficient
                .try_reserve_exact(1 + usize::from(negative))
                .map_err(|_| NumericError::Allocation)?;
            if negative {
                coefficient.push('-');
            }
            coefficient.push('0');
        }
        Some(first) => {
            let retained = combined.clone().skip(first);
            let retained_len = int_digits.len() + fraction_digits.len() - first;
            coefficient
                .try_reserve_exact(retained_len + usize::from(negative))
                .map_err(|_| NumericError::Allocation)?;
            if negative {
                coefficient.push('-');
            }
            coefficient.extend(retained.map(char::from));
        }
    }
    Ok(Some((coefficient, scale)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    /// The canonical validator's own contract: a `-0` coefficient is rejected at any nonzero scale (it would render
    /// `-0.000`, a canonical decimal with untrimmed trailing zeroes) while the retained `-0` at scale zero stays legal
    /// — the signed-zero spelling the strict JSON decode retains.
    #[test]
    fn canonical_parts_reject_negative_zero_at_nonzero_scale() {
        assert!(Decimal::validate_canonical_parts("-0", 3).is_err());
        assert!(Decimal::validate_canonical_parts("-0", 0).is_ok());
    }

    /// `decimal_parts_to_f64` takes a borrowed coefficient at the crate root, so it must not subtract `'0'` from
    /// whatever byte it is handed: text the bounded accumulator cannot read falls through to the parse below, which
    /// answers `NaN` for it instead of wrapping a mantissa.
    #[test]
    fn a_non_numeric_coefficient_leaves_the_bounded_accumulator() {
        assert!(super::decimal_parts_to_f64("abc", 0).is_nan());
        assert!(super::decimal_parts_to_f64("1 2", 0).is_nan());
        assert!(super::decimal_parts_to_f64("-x", 0).is_nan());
    }

    /// `decimal_parts_to_f64` is byte-identical to the `format!().parse()` round trip it replaced (same bytes, same std
    /// parser), across signs, scales, and coefficient widths — the equivalence pin the contagion paths rest on. `-0`
    /// and the `i64::MIN` scale are the two deliberate deviations from that identity: a retained `-0` coefficient keeps
    /// its negative sign, and `-i64::MIN`'s scale spells the exponent 2^63 — both asserted below.
    #[test]
    fn decimal_parts_to_f64_matches_the_string_round_trip() {
        for (coefficient, scale) in [
            ("1", 0),
            ("15", 1),
            ("-15", 1),
            ("1", 6),
            ("1", -400),
            ("1000", 3),
            ("123456789", 8),
            ("-123456789012345", 12),
            ("9999999999999999999", 0),
            ("0", 1),
            ("0", -5),
            ("10", 1),
        ] {
            let direct = decimal_parts_to_f64(coefficient, scale);
            let round_trip = format!("{coefficient}e{}", -scale).parse::<f64>().unwrap_or(f64::NAN);
            assert_eq!(direct.to_bits(), round_trip.to_bits(), "{coefficient}e{scale}");
        }
        // A retained `-0` coefficient converts to a NEGATIVE zero — the decimal-to-double law (`-0.0 | floor` is
        // `-0`), pinned by the corpus.
        assert_eq!(decimal_parts_to_f64("-0", 5).to_bits(), (-0.0_f64).to_bits());
        // `scale = i64::MIN` is the exponent 2^63: astronomically out of range, so the correctly-parsed answer is
        // positive infinity — where the old `-i64::MIN` wrapped (debug: panicked).
        assert_eq!(decimal_parts_to_f64("1", i64::MIN).to_bits(), f64::INFINITY.to_bits());
    }

    fn assert_decimal(spelling: &str, coefficient: &str, scale: i64) {
        let decimal = Decimal::parse(spelling).expect("fixture decimal parses");
        assert_eq!(decimal.coefficient().as_str(), coefficient);
        assert_eq!(decimal.scale(), scale);
    }

    /// The machine fast path must reproduce the canonicalization law exactly: a zero between nonzeros is a VALUE digit
    /// (`10.5` → 105), leading zeros vanish, trailing zeros trim the scale, and the overflow boundary falls back to
    /// the arbitrary-precision arm with an identical answer.
    #[test]
    fn parse_machine_fast_path_matches_the_canonical_law() {
        for (spelling, coefficient, scale) in [
            ("10.5", "105", 1),    // middle zero is a value digit
            ("1005", "1005", 0),   // interior zeros survive
            ("0001500", "15", -2), // leading and trailing zeros
            ("1.50", "15", 1),
            ("0.00150", "15", 4),
            ("0.000", "0", 0),
            ("-0.0", "0", 0),
            ("9223372036854775807", "9223372036854775807", 0), // i64::MAX fits
            ("92233720368547758.07", "9223372036854775807", 2),
            ("-9223372036854775807.5", "-92233720368547758075", 1),
            ("10e2", "1", -3),
            ("0.001e3", "1", 0),
            // Beyond i64: the fallback arm answers identically.
            ("9223372036854775808", "9223372036854775808", 0),
            ("123456789012345678901234567890.5", "1234567890123456789012345678905", 1),
        ] {
            assert_decimal(spelling, coefficient, scale);
        }
    }

    #[test]
    fn parse_preserves_canonical_zero_and_digit_normalization() {
        assert_decimal("-000.000", "0", 0);
        assert_decimal("+00012.34000e+2", "1234", 0);
        assert_decimal(".5000", "5", 1);
        assert_decimal("1200.", "12", -2);
    }

    #[test]
    fn parse_rejects_malformed_mantissas_and_exponents() {
        for (spelling, expected) in [
            ("", NumericError::Empty),
            ("+", NumericError::Empty),
            ("-", NumericError::Empty),
            (".", NumericError::Empty),
            ("+.", NumericError::Empty),
            ("1..0", NumericError::InvalidDigit),
            ("1a", NumericError::InvalidDigit),
            ("1e", NumericError::InvalidExponent),
            ("1e+", NumericError::InvalidExponent),
            ("1e2e3", NumericError::InvalidExponent),
            (
                "1e170141183460469231731687303715884105728",
                NumericError::InvalidExponent,
            ),
        ] {
            assert_eq!(Decimal::parse(spelling), Err(expected), "{spelling:?}");
        }
    }

    #[test]
    fn parse_checks_scale_boundaries_after_trailing_zero_normalization() {
        assert_decimal("1e9223372036854775808", "1", i64::MIN);
        assert_decimal("1e-9223372036854775807", "1", i64::MAX);
        assert_decimal("10e9223372036854775807", "1", i64::MIN);
        assert_decimal("10e-9223372036854775808", "1", i64::MAX);
        assert_eq!(
            Decimal::parse("1e9223372036854775809"),
            Err(NumericError::ExponentOverflow)
        );
        assert_eq!(
            Decimal::parse("1e-9223372036854775808"),
            Err(NumericError::ExponentOverflow)
        );
    }

    /// The part-constructor laws: [`Decimal::from_parts`] demands an already-canonical coefficient (a zero magnitude
    /// pairs only with scale 0 and a longer magnitude carries no trailing zero), [`Decimal::from_parts_normalizing`] is
    /// the materialization that DOES trim into canonical form, and [`Decimal::from_literal_parts`] is the
    /// retained-spelling constructor that keeps exactly what `from_parts` refuses.
    #[test]
    fn part_constructors_enforce_the_canonical_and_literal_grammars() {
        // `from_parts`: canonical only.
        assert!(Decimal::from_parts(Integer::from_i64(1), 0).is_ok());
        assert!(Decimal::from_parts(Integer::from_i64(1025), 2).is_ok());
        assert!(Decimal::from_parts(Integer::from_i64(0), 0).is_ok());
        assert_eq!(
            Decimal::from_parts(Integer::from_i64(0), 3),
            Err(NumericError::InvalidDigit),
            "a zero magnitude pairs only with scale 0"
        );
        assert_eq!(
            Decimal::from_parts(Integer::from_i64(1000), 2),
            Err(NumericError::InvalidDigit),
            "trailing zeroes are not canonical"
        );

        // `from_parts_normalizing`: value `coefficient * 10^-scale` is preserved while trailing zeroes are trimmed and
        // a zero magnitude collapses to the canonical zero.
        let normalized = |coefficient: &str, scale: i64| {
            let value = Decimal::from_parts_normalizing(
                Integer::from_canonical(String::from(coefficient)).expect("canonical"),
                scale,
            )
            .expect("normalizes");
            (String::from(value.coefficient().as_str()), value.scale())
        };
        assert_eq!(normalized("1000", 3), (String::from("1"), 0));
        assert_eq!(normalized("10250", 3), (String::from("1025"), 2));
        assert_eq!(normalized("1200", 5), (String::from("12"), 3));
        assert_eq!(normalized("0", 5), (String::from("0"), 0));
        assert_eq!(normalized("-0", 7), (String::from("0"), 0));

        // `from_literal_parts`: the retained constructor keeps trailing zeroes and a zero magnitude at any scale, so a
        // literal renders byte-exact.
        let kept = Decimal::from_literal_parts(Integer::from_canonical(String::from("1000")).expect("canonical"), 3)
            .expect("trailing zeroes are literal-legal");
        assert_eq!(kept.coefficient().as_str(), "1000");
        assert_eq!(kept.scale(), 3);
        let kept_zero = Decimal::from_literal_parts(Integer::from_canonical(String::from("0")).expect("canonical"), -5)
            .expect("zero at any scale is literal-legal");
        assert_eq!(kept_zero.scale(), -5);
    }
}
