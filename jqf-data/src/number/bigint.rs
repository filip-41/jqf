//! Scratch arbitrary-precision integer for exact arithmetic.
//!
//! Add, subtract, multiply, truncating divide/remainder, gcd, powers of ten, and factor-of-two/five extraction.
//! Magnitude is little-endian base-ten digits so conversion to [`crate::Integer`] is direct. Zero is an empty magnitude
//! with a non-negative sign.
//!
//! Not a [`crate::Number`] payload. The caller drives the operations and charges the growth. Not a general bignum: no
//! bitwise ops, no modular exponentiation, schoolbook `O(n^2)` multiply and divide. Allocates ordinary `Vec`s and never
//! touches the document.

#![allow(
    clippy::cast_possible_truncation,
    reason = "base-ten digits and small carries provably fit `u8`/`u16` in this bignum kernel"
)]
#![allow(
    clippy::needless_range_loop,
    reason = "the schoolbook magnitude routines index two aligned digit slices in lockstep, \
              which reads more directly than a zipped reverse iterator"
)]

use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

/// A signed arbitrary-precision integer as sign plus little-endian base-ten digits. The magnitude never carries
/// high-order zero digits; zero is the empty magnitude with `negative == false`.
///
/// Equality and ordering are by mathematical value: the representation is canonical, so the derived structural equality
/// and the manual [`Ord`] agree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BigInt {
    negative: bool,
    /// Little-endian base-ten digits (`digits[0]` is the ones place). Empty is zero.
    digits: Vec<u8>,
}

/// Largest shifted magnitude [`BigInt::mul_pow10`] will build: `1 << 40` digits.
///
/// Asking the allocator for ~2^62 bytes refuses cleanly on native allocators but SIGTRAPs under Rosetta 2, so a
/// 2^63-scale decimal (`1e-9223372036854775807`) is refused here on every platform. A request that can actually run is
/// far below this; the caller charges operand sizes first.
///
/// Spelled target-width-aware because `1 << 40` does not fit a 32-bit `usize` (wasm32): there the ceiling becomes half
/// the address space, still far above any allocable digit vector, so the refusal law is unchanged on both.
const MAX_MAGNITUDE_DIGITS: usize = if usize::BITS >= 64 { 1 << 40 } else { usize::MAX >> 1 };

impl BigInt {
    /// The canonical zero.
    #[must_use]
    pub fn zero() -> Self {
        Self {
            negative: false,
            digits: Vec::new(),
        }
    }

    /// A small non-negative integer.
    #[must_use]
    pub fn from_u64(mut value: u64) -> Self {
        let mut digits = Vec::new();
        while value != 0 {
            digits.push((value % 10) as u8);
            value /= 10;
        }
        Self {
            negative: false,
            digits,
        }
    }

    /// Parses signed base-ten digit text (`-?[0-9]+`). Redundant leading zeroes are dropped (`007` → `7`) and an
    /// accepted `-0` normalizes to zero — strictness over leading zeroes lives upstream; this accepts every digit
    /// run. Returns `None` for any non-digit content.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (negative, digits_text) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text),
        };
        if digits_text.is_empty() {
            return None;
        }
        let mut digits = Vec::new();
        for byte in digits_text.bytes().rev() {
            if !byte.is_ascii_digit() {
                return None;
            }
            digits.push(byte - b'0');
        }
        trim(&mut digits);
        Some(Self::normalized(negative, digits))
    }

    fn normalized(negative: bool, mut digits: Vec<u8>) -> Self {
        trim(&mut digits);
        Self {
            negative: negative && !digits.is_empty(),
            digits,
        }
    }

    /// Whether the value is zero.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.digits.is_empty()
    }

    /// Whether the value is strictly negative.
    #[must_use]
    pub fn is_negative(&self) -> bool {
        self.negative
    }

    /// Renders canonical signed decimal text.
    #[must_use]
    pub fn to_decimal_string(&self) -> String {
        if self.digits.is_empty() {
            return String::from("0");
        }
        let mut out = String::with_capacity(self.digits.len() + usize::from(self.negative));
        if self.negative {
            out.push('-');
        }
        for &digit in self.digits.iter().rev() {
            out.push(char::from(b'0' + digit));
        }
        out
    }

    /// The negation.
    #[must_use]
    pub fn negate(&self) -> Self {
        Self::normalized(!self.negative, self.digits.clone())
    }

    /// The absolute value.
    #[must_use]
    pub fn abs(&self) -> Self {
        Self::normalized(false, self.digits.clone())
    }

    /// Signed addition.
    #[must_use]
    pub fn add(&self, other: &Self) -> Self {
        if self.negative == other.negative {
            Self::normalized(self.negative, add_magnitude(&self.digits, &other.digits))
        } else {
            match cmp_magnitude(&self.digits, &other.digits) {
                Ordering::Equal => Self::zero(),
                Ordering::Greater => Self::normalized(self.negative, sub_magnitude(&self.digits, &other.digits)),
                Ordering::Less => Self::normalized(other.negative, sub_magnitude(&other.digits, &self.digits)),
            }
        }
    }

    /// Signed subtraction.
    #[must_use]
    pub fn sub(&self, other: &Self) -> Self {
        self.add(&other.negate())
    }

    /// Signed multiplication.
    #[must_use]
    pub fn mul(&self, other: &Self) -> Self {
        if self.is_zero() || other.is_zero() {
            return Self::zero();
        }
        Self::normalized(
            self.negative != other.negative,
            mul_magnitude(&self.digits, &other.digits),
        )
    }

    /// Truncating division and remainder (quotient truncates toward zero; the remainder takes the dividend's sign), or
    /// `None` when `divisor` is zero.
    #[must_use]
    pub fn div_rem(&self, divisor: &Self) -> Option<(Self, Self)> {
        if divisor.is_zero() {
            return None;
        }
        let (quotient_mag, remainder_mag) = divmod_magnitude(&self.digits, &divisor.digits);
        let quotient = Self::normalized(self.negative != divisor.negative, quotient_mag);
        let remainder = Self::normalized(self.negative, remainder_mag);
        Some((quotient, remainder))
    }

    /// Multiplies by `10^power` (a decimal left shift), or `None` when the shifted magnitude cannot be built — the
    /// capacity would overflow, the magnitude exceeds [`MAX_MAGNITUDE_DIGITS`], or the allocator refuses. The
    /// exact-number engine maps a `None` onto its defensive out-of-range class, so an absurd shift (a retained
    /// literal's 2^63-scale gap) is a clean refusal, never an allocation panic.
    #[must_use]
    pub fn mul_pow10(&self, power: usize) -> Option<Self> {
        if self.is_zero() || power == 0 {
            return Some(self.clone());
        }
        let len = self.digits.len().checked_add(power)?;
        if len > MAX_MAGNITUDE_DIGITS {
            return None;
        }
        let mut digits = Vec::new();
        digits.try_reserve_exact(len).ok()?;
        digits.resize(power, 0);
        digits.extend_from_slice(&self.digits);
        Some(Self::normalized(self.negative, digits))
    }

    /// `10^power` as a non-negative value, or `None` when the magnitude cannot be built (see [`Self::mul_pow10`]).
    #[must_use]
    pub fn pow10(power: usize) -> Option<Self> {
        Self::from_u64(1).mul_pow10(power)
    }

    /// The number of base-ten digits of the magnitude (zero for the value zero). The exact comparison and truncation
    /// laws read it to answer by magnitude and to decide when a fraction truncates to zero without materializing a
    /// `10^scale` divisor.
    #[must_use]
    pub fn digit_count(&self) -> usize {
        self.digits.len()
    }

    /// Compares this magnitude against `other` when each is padded with its given count of LOW-order zero digits —
    /// the alignment of two `coefficient * 10^-scale` pairs whose adjusted exponents are equal, in which case the two
    /// padded lengths coincide and the scale gap equals the digit-count gap. Padding is pure indexing: no shifted
    /// magnitude is materialized, so the comparison cannot allocate at all.
    ///
    /// The padded lengths carry [`Self::mul_pow10`]'s refusal law: either one overflowing `usize`, or exceeding
    /// [`MAX_MAGNITUDE_DIGITS`], answers `None` instead of silently wrapping or comparing against a length no shifted
    /// magnitude could have had.
    #[must_use]
    pub fn cmp_aligned_padded(&self, self_pad: usize, other: &Self, other_pad: usize) -> Option<Ordering> {
        let len = self
            .digits
            .len()
            .checked_add(self_pad)?
            .max(other.digits.len().checked_add(other_pad)?);
        if len > MAX_MAGNITUDE_DIGITS {
            return None;
        }
        for index in (0..len).rev() {
            // A padded position below `pad` is a shifted-in zero; above it the digit is the original magnitude's, and
            // beyond the original length the position is a high-order zero.
            let left = if index < self_pad {
                0
            } else {
                self.digits.get(index - self_pad).copied().unwrap_or(0)
            };
            let right = if index < other_pad {
                0
            } else {
                other.digits.get(index - other_pad).copied().unwrap_or(0)
            };
            match left.cmp(&right) {
                Ordering::Equal => {}
                other => return Some(other),
            }
        }
        Some(Ordering::Equal)
    }

    /// The greatest common divisor of two magnitudes (sign ignored); `gcd(0, x)` is `x`.
    ///
    /// # Panics
    ///
    /// Never in practice: the loop condition guarantees the divisor handed to `div_rem` is non-zero.
    #[must_use]
    pub fn gcd(&self, other: &Self) -> Self {
        let mut a = self.abs();
        let mut b = other.abs();
        while !b.is_zero() {
            let (_, remainder) = a.div_rem(&b).expect("nonzero divisor in gcd");
            a = b;
            b = remainder.abs();
        }
        a
    }

    /// Extracts every factor of `prime` (2 or 5) from a non-zero magnitude, returning the count removed and the reduced
    /// value. On zero it returns `(0, zero)`, and on a base below two `(0, magnitude)`.
    #[must_use]
    pub fn factor_out(&self, prime: u8) -> (u32, Self) {
        if self.is_zero() {
            return (0, Self::zero());
        }
        // Only a base of two or more divides down to a fixed point: zero has no quotient at all, and one never leaves a
        // remainder, so neither has a factor count to report.
        if prime < 2 {
            return (0, self.abs());
        }
        let mut value = self.abs();
        let mut count = 0;
        loop {
            let (quotient, remainder) = value.div_rem_small(prime);
            if !remainder.is_zero() {
                break;
            }
            value = quotient;
            count += 1;
        }
        (count, value)
    }

    /// Divides the magnitude by a small non-zero base-ten value, returning the quotient and remainder as [`BigInt`]s
    /// (both non-negative).
    fn div_rem_small(&self, divisor: u8) -> (Self, Self) {
        let mut quotient = alloc::vec![0u8; self.digits.len()];
        let mut remainder: u32 = 0;
        for index in (0..self.digits.len()).rev() {
            let current = remainder * 10 + u32::from(self.digits[index]);
            quotient[index] = (current / u32::from(divisor)) as u8;
            remainder = current % u32::from(divisor);
        }
        (Self::normalized(false, quotient), Self::from_u64(u64::from(remainder)))
    }
}

impl Ord for BigInt {
    /// Total order by mathematical value.
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.negative, other.negative) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => cmp_magnitude(&self.digits, &other.digits),
            (true, true) => cmp_magnitude(&other.digits, &self.digits),
        }
    }
}

impl PartialOrd for BigInt {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Removes high-order zero digits so the magnitude stays canonical.
fn trim(digits: &mut Vec<u8>) {
    while digits.last() == Some(&0) {
        digits.pop();
    }
}

fn cmp_magnitude(a: &[u8], b: &[u8]) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.iter().rev().cmp(b.iter().rev()))
}

fn add_magnitude(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len().max(b.len()) + 1);
    let mut carry = 0u8;
    let mut index = 0;
    while index < a.len() || index < b.len() || carry != 0 {
        let sum = carry + a.get(index).copied().unwrap_or(0) + b.get(index).copied().unwrap_or(0);
        out.push(sum % 10);
        carry = sum / 10;
        index += 1;
    }
    out
}

/// Subtracts `b` from `a`, requiring `a >= b` (checked by the caller).
fn sub_magnitude(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i8;
    for index in 0..a.len() {
        let mut digit = i8::try_from(a[index]).unwrap_or(0)
            - borrow
            - i8::try_from(b.get(index).copied().unwrap_or(0)).unwrap_or(0);
        if digit < 0 {
            digit += 10;
            borrow = 1;
        } else {
            borrow = 0;
        }
        out.push(u8::try_from(digit).unwrap_or(0));
    }
    out
}

fn mul_magnitude(a: &[u8], b: &[u8]) -> Vec<u8> {
    // The intermediate digits stay below 10 at every read (`current` is at most 9 + 9*9 + 9), so a single u8 buffer
    // replaces the previous u16 staging plus a conversion pass.
    let mut out = alloc::vec![0u8; a.len() + b.len()];
    for (i, &da) in a.iter().enumerate() {
        let mut carry = 0u8;
        for (j, &db) in b.iter().enumerate() {
            let current = out[i + j] + da * db + carry;
            out[i + j] = current % 10;
            carry = current / 10;
        }
        let mut k = i + b.len();
        while carry != 0 {
            let current = out[k] + carry;
            out[k] = current % 10;
            carry = current / 10;
            k += 1;
        }
    }
    // Trim high-order zeros so the product stays canonical (a zero product must be the empty magnitude, not a run of
    // zero digits — `cmp_magnitude` and the long-division search compare by length first).
    trim(&mut out);
    out
}

/// Long division of magnitudes, returning `(quotient, remainder)`; `divisor` must be non-zero.
fn divmod_magnitude(dividend: &[u8], divisor: &[u8]) -> (Vec<u8>, Vec<u8>) {
    if cmp_magnitude(dividend, divisor) == Ordering::Less {
        return (Vec::new(), dividend.to_vec());
    }
    let mut quotient = alloc::vec![0u8; dividend.len()];
    let mut remainder: Vec<u8> = Vec::new();
    for index in (0..dividend.len()).rev() {
        // remainder = remainder * 10 + next digit (little-endian: prepend).
        remainder.insert(0, dividend[index]);
        trim(&mut remainder);
        // The largest q in 0..=9 with divisor * q <= remainder. A linear scan over nine candidates; the schoolbook
        // division around it dominates.
        let digit = (1..=9u8)
            .rev()
            .find(|&q| cmp_magnitude(&mul_magnitude(divisor, &[q]), &remainder).is_le())
            .unwrap_or(0);
        quotient[index] = digit;
        let product = mul_magnitude(divisor, &[digit]);
        remainder = sub_magnitude(&remainder, &product);
        trim(&mut remainder);
    }
    trim(&mut quotient);
    (quotient, remainder)
}

#[cfg(test)]
mod tests {
    use super::BigInt;
    use core::cmp::Ordering;

    fn big(text: &str) -> BigInt {
        BigInt::parse(text).expect("parses")
    }

    #[test]
    fn parses_and_renders_canonically() {
        assert_eq!(big("0").to_decimal_string(), "0");
        assert_eq!(big("-0").to_decimal_string(), "0");
        assert_eq!(big("12300").to_decimal_string(), "12300");
        assert_eq!(big("-45").to_decimal_string(), "-45");
        assert_eq!(
            big("123456789012345678901234567890").to_decimal_string(),
            "123456789012345678901234567890"
        );
    }

    /// `factor_out` is public, so a base outside the documented two-or-five must terminate with an answer instead of
    /// dividing by zero or spinning forever on the unit that never leaves a remainder.
    #[test]
    fn factoring_out_a_base_below_two_removes_nothing() {
        assert_eq!(big("-1200").factor_out(1).0, 0);
        assert_eq!(big("-1200").factor_out(1).1.to_decimal_string(), "1200");
        assert_eq!(big("1200").factor_out(0).0, 0);
        assert_eq!(big("1200").factor_out(0).1.to_decimal_string(), "1200");
        // The documented bases still count and reduce.
        assert_eq!(big("1200").factor_out(2).0, 4);
        assert_eq!(big("1200").factor_out(5).1.to_decimal_string(), "48");
    }

    #[test]
    fn add_and_sub_cover_sign_combinations() {
        assert_eq!(big("999").add(&big("1")).to_decimal_string(), "1000");
        assert_eq!(big("5").add(&big("-8")).to_decimal_string(), "-3");
        assert_eq!(big("-5").add(&big("8")).to_decimal_string(), "3");
        assert_eq!(big("5").sub(&big("8")).to_decimal_string(), "-3");
        assert_eq!(big("10").sub(&big("10")).to_decimal_string(), "0");
        assert_eq!(
            big("10000000000000000000").add(&big("1")).to_decimal_string(),
            "10000000000000000001"
        );
    }

    #[test]
    fn multiply_matches_known_products() {
        assert_eq!(big("12").mul(&big("12")).to_decimal_string(), "144");
        assert_eq!(big("-7").mul(&big("6")).to_decimal_string(), "-42");
        assert_eq!(big("0").mul(&big("999")).to_decimal_string(), "0");
        assert_eq!(
            big("9007199254740993").mul(&big("2")).to_decimal_string(),
            "18014398509481986"
        );
    }

    #[test]
    fn div_rem_truncates_toward_zero_with_dividend_sign() {
        let (q, r) = big("17").div_rem(&big("5")).expect("nonzero");
        assert_eq!((q.to_decimal_string(), r.to_decimal_string()), ("3".into(), "2".into()));
        let (q, r) = big("-10").div_rem(&big("3")).expect("nonzero");
        assert_eq!(
            (q.to_decimal_string(), r.to_decimal_string()),
            ("-3".into(), "-1".into())
        );
        let (q, r) = big("10").div_rem(&big("-3")).expect("nonzero");
        assert_eq!(
            (q.to_decimal_string(), r.to_decimal_string()),
            ("-3".into(), "1".into())
        );
        assert!(big("5").div_rem(&big("0")).is_none());
    }

    #[test]
    fn gcd_and_factoring() {
        assert_eq!(big("12").gcd(&big("18")).to_decimal_string(), "6");
        assert_eq!(big("0").gcd(&big("7")).to_decimal_string(), "7");
        let (twos, rest) = big("40").factor_out(2);
        assert_eq!((twos, rest.to_decimal_string()), (3, "5".into()));
        let (fives, rest) = big("50").factor_out(5);
        assert_eq!((fives, rest.to_decimal_string()), (2, "2".into()));
    }

    #[test]
    fn powers_of_ten_and_ordering() {
        assert_eq!(BigInt::pow10(3).expect("builds").to_decimal_string(), "1000");
        assert_eq!(big("7").mul_pow10(2).expect("builds").to_decimal_string(), "700");
        // The zero-pad alignment is the signed magnitude order the exact comparison reads for a same-scale tie.
        let aligned = |a: &str, a_pad: usize, b: &str, b_pad: usize| {
            big(a)
                .cmp_aligned_padded(a_pad, &big(b), b_pad)
                .expect("test pads stay under the magnitude ceiling")
        };
        assert_eq!(aligned("100", 0, "99", 0), Ordering::Greater);
        assert_eq!(aligned("42", 0, "42", 0), Ordering::Equal);
    }

    /// An absurd shift is a clean `None`, never an allocation panic: the retained-literal class
    /// `1e-9223372036854775807` aligns by a ~2^63-digit gap, which must refuse rather than build.
    #[test]
    fn an_absurd_shift_refuses_cleanly() {
        assert!(big("1").mul_pow10(1 << 62).is_none());
        assert!(BigInt::pow10(1 << 62).is_none());
        assert!(big("1").mul_pow10(usize::MAX).is_none());
        assert_eq!(big("7").mul_pow10(0).expect("zero shift").to_decimal_string(), "7");
        assert!(big("0").mul_pow10(1 << 62).is_some());
    }

    /// The padded alignment the exact comparison rests on: padding is pure indexing, so equal-adjusted-exponent pairs
    /// compare exactly without materializing a shifted magnitude.
    #[test]
    fn cmp_aligned_padded_matches_the_shifted_compare() {
        let aligned = |a: &str, a_pad: usize, b: &str, b_pad: usize| {
            big(a)
                .cmp_aligned_padded(a_pad, &big(b), b_pad)
                .expect("test pads stay under the magnitude ceiling")
        };
        // 150 (scale 2) vs 15 padded by one (scale 1): equal values.
        assert_eq!(aligned("150", 0, "15", 1), Ordering::Equal);
        // 150 vs 16 padded by one: 150 > 160 is false, so Less.
        assert_eq!(aligned("150", 0, "16", 1), Ordering::Less);
        // A padded operand longer than the other's original digits: the extra positions read as high-order zeros.
        assert_eq!(aligned("1", 2, "100", 0), Ordering::Equal);
        assert_eq!(aligned("1", 1, "9", 0), Ordering::Greater);
        assert_eq!(aligned("10000", 0, "1", 4), Ordering::Equal);
        assert_eq!(aligned("12345", 0, "1", 4), Ordering::Greater);
    }

    /// A pad whose aligned length wraps `usize` or passes [`MAX_MAGNITUDE_DIGITS`] refuses with `None` instead of
    /// comparing against a wrapped length — the same clean refusal `mul_pow10` gives an absurd shift.
    #[test]
    fn an_absurd_pad_refuses_cleanly() {
        assert_eq!(big("1").cmp_aligned_padded(usize::MAX, &big("1"), 0), None);
        assert_eq!(big("1").cmp_aligned_padded(1 << 62, &big("0"), 0), None);
    }
}
