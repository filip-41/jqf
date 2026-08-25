//! Codec-local arbitrary-precision signed integers.
//!
//! CBOR bignums (tags 2/3), tag-4 mantissas, and tag-1 float fractions need integer arithmetic beyond the machine
//! types. This is a minimal base-2^32 little-endian magnitude integer with the exact operations the decode needs:
//! big-endian bytes in, canonical decimal text out, multiplication by small factors and by other bignums, and shifts by
//! whole bits. It is format-neutral (`no_std` + `alloc`) and lives here rather than in `jqf-data` because it is the
//! codec's own wire arithmetic, not a document semantic.

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write as _;

/// The decimal divisor for repeated conversion: `10^9` is the largest power of ten whose square fits in `u64` with the
/// base-`2^32` carry law (`divisor * 2^32 < 2^64`), so one divide step never overflows and never emits a quotient limb
/// above `u32::MAX`.
const DECIMAL_BASE: u32 = 1_000_000_000;

/// One exact signed integer. The magnitude is little-endian base-`2^32` limbs with no leading zero limb; an empty limb
/// vector is the magnitude zero. The sign is separate; zero carries `neg == false`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Big {
    neg: bool,
    limbs: Vec<u32>,
}

impl Big {
    /// The exact value zero.
    pub(crate) const fn zero() -> Self {
        Self {
            neg: false,
            limbs: Vec::new(),
        }
    }

    /// The exact value one.
    pub(crate) fn one() -> Self {
        Self {
            neg: false,
            limbs: vec![1],
        }
    }

    /// Builds the value of a big-endian unsigned byte magnitude.
    pub(crate) fn from_big_endian(bytes: &[u8]) -> Self {
        // Each four-byte group is a base-2^32 digit. `rchunks` groups from the END, so it yields the limbs
        // least-significant first — the array's own little-endian order — and leaves the PARTIAL group (the most
        // significant 1-3 bytes) last, where it belongs.
        let mut limbs: Vec<u32> = bytes
            .rchunks(4)
            .map(|group| group.iter().fold(0u32, |limb, &byte| (limb << 8) | u32::from(byte)))
            .collect();
        while limbs.last() == Some(&0) {
            limbs.pop();
        }
        Self { neg: false, limbs }
    }

    /// Builds the value of an unsigned machine integer.
    pub(crate) fn from_u64(value: u64) -> Self {
        Self::from_big_endian(&value.to_be_bytes())
    }

    /// Whether the value is exactly zero.
    #[must_use]
    pub(crate) fn is_zero(&self) -> bool {
        self.limbs.is_empty()
    }

    /// The canonical magnitude limbs, least-significant first (no leading zero limb), so equal magnitudes always
    /// compare and hash limb-identically.
    #[must_use]
    pub(crate) fn limbs(&self) -> &[u32] {
        &self.limbs
    }

    /// Whether the value is negative.
    #[must_use]
    pub(crate) fn is_negative(&self) -> bool {
        self.neg && !self.is_zero()
    }

    /// The value with the opposite sign.
    pub(crate) fn negated(mut self) -> Self {
        if !self.is_zero() {
            self.neg = !self.neg;
        }
        self
    }

    /// The exact product of this magnitude and a small unsigned factor.
    pub(crate) fn mul_small(&self, factor: u32) -> Self {
        if self.is_zero() || factor == 0 {
            return Self::zero();
        }
        if factor == 1 {
            return self.clone();
        }
        let mut out = vec![0u32; self.limbs.len() + 1];
        let mut carry: u64 = 0;
        for (index, limb) in self.limbs.iter().enumerate() {
            let product = u64::from(*limb) * u64::from(factor) + carry;
            out[index] = product as u32;
            carry = product >> 32;
        }
        out[self.limbs.len()] = carry as u32;
        while out.last() == Some(&0) {
            out.pop();
        }
        Self {
            neg: self.neg,
            limbs: out,
        }
    }

    /// The exact product of two bignums.
    pub(crate) fn mul(&self, other: &Self) -> Self {
        if self.is_zero() || other.is_zero() {
            return Self::zero();
        }
        let a = &self.limbs;
        let b = &other.limbs;
        let mut out = vec![0u32; a.len() + b.len()];
        for (i, ai) in a.iter().enumerate() {
            let mut carry: u64 = 0;
            for (j, bj) in b.iter().enumerate() {
                let product = u64::from(*ai) * u64::from(*bj) + carry + u64::from(out[i + j]);
                out[i + j] = product as u32;
                carry = product >> 32;
            }
            out[i + b.len()] = carry as u32;
        }
        while out.last() == Some(&0) {
            out.pop();
        }
        Self {
            neg: self.neg != other.neg,
            limbs: out,
        }
    }

    /// The exact quotient and remainder of division by a small `u32` divisor.
    ///
    /// `divisor` must be nonzero. The remainder is over the magnitude, so `-m % divisor == m % divisor` for the
    /// divisibility checks §5.6.1's decimal/bigfloat normalization performs (only zero-ness matters there).
    pub(crate) fn div_rem_small(&self, divisor: u32) -> (Self, u32) {
        debug_assert!(divisor != 0);
        let mut magnitude = self.limbs.clone();
        let mut remainder: u64 = 0;
        for limb in magnitude.iter_mut().rev() {
            let current = (remainder << 32) | u64::from(*limb);
            *limb = (current / u64::from(divisor)) as u32;
            remainder = current % u64::from(divisor);
        }
        while magnitude.last() == Some(&0) {
            magnitude.pop();
        }
        (
            Self {
                neg: self.neg,
                limbs: magnitude,
            },
            remainder as u32,
        )
    }

    /// Whether the magnitude is even.
    #[must_use]
    pub(crate) fn is_even(&self) -> bool {
        self.limbs.first().is_none_or(|limb| limb & 1 == 0)
    }

    /// The exact sum of the value and a small nonnegative addend.
    pub(crate) fn add_small(&self, addend: u32) -> Self {
        if self.is_zero() {
            return Self::from_u64(u64::from(addend));
        }
        if self.neg {
            // `-m + a`: when `a <= m` the result is negative `-(m - a)`; otherwise it is positive `a - m`. Magnitude
            // subtraction is done limb-wise with borrows; a surviving borrow means the addend exceeded the magnitude,
            // and the wrapped limbs hold `addend - m` reduced mod `2^(32n)` — negating them recovers the exact
            // positive difference.
            let mut limbs = self.limbs.clone();
            let mut borrow = u64::from(addend);
            for limb in &mut limbs {
                let (current, carry) = u64::from(*limb).overflowing_sub(borrow);
                *limb = current as u32;
                borrow = carry as u64;
                if borrow == 0 {
                    break;
                }
            }
            if borrow != 0 {
                let mut carry = 1u64;
                for limb in &mut limbs {
                    let sum = u64::from(!*limb) + carry;
                    *limb = sum as u32;
                    carry = sum >> 32;
                    if carry == 0 {
                        break;
                    }
                }
                return Self { neg: false, limbs };
            }
            while limbs.last() == Some(&0) {
                limbs.pop();
            }
            Self {
                neg: !limbs.is_empty(),
                limbs,
            }
        } else {
            let mut limbs = self.limbs.clone();
            let mut carry = u64::from(addend);
            for limb in &mut limbs {
                // The carry is the part beyond the 32-bit limb, NOT a u64 overflow: adding to a widened limb can fit
                // u64 while still carrying out of the u32 truncation (e.g. `0xffffffff + 1`).
                let sum = u64::from(*limb) + carry;
                *limb = sum as u32;
                carry = sum >> 32;
                if carry == 0 {
                    break;
                }
            }
            if carry != 0 {
                limbs.push(carry as u32);
            }
            Self { neg: false, limbs }
        }
    }

    /// The exact signed value as `i64`, or `None` when it does not fit.
    #[must_use]
    pub(crate) fn to_i64(&self) -> Option<i64> {
        if self.is_zero() {
            return Some(0);
        }
        if self.limbs.len() > 2 {
            return None;
        }
        let low = self.limbs[0] as u64;
        let high = self.limbs.get(1).copied().unwrap_or(0) as u64;
        let magnitude = low | (high << 32);
        if self.neg {
            // The value is `-m`; it fits when `m <= 2^63`, mapping exactly to `i64::MIN` at the bound.
            match magnitude.cmp(&(1u64 << 63)) {
                core::cmp::Ordering::Greater => None,
                core::cmp::Ordering::Equal => Some(i64::MIN),
                core::cmp::Ordering::Less => Some(-(magnitude as i64)),
            }
        } else {
            i64::try_from(magnitude).ok()
        }
    }

    /// The exact value as `u64`, or `None` when it is negative or does not fit.
    #[must_use]
    pub(crate) fn to_u64(&self) -> Option<u64> {
        if self.is_zero() {
            return Some(0);
        }
        if self.neg || self.limbs.len() > 2 {
            return None;
        }
        let low = self.limbs[0] as u64;
        let high = self.limbs.get(1).copied().unwrap_or(0) as u64;
        Some(low | (high << 32))
    }

    /// The exact difference of the value and a small nonnegative subtrahend.
    ///
    /// `subtrahend` must be small enough that `value - subtrahend` does not cross zero in a way the magnitude
    /// arithmetic mishandles; the encoder uses it for `m - 1` on magnitudes strictly above the u64 range.
    pub(crate) fn sub_small(&self, subtrahend: u32) -> Self {
        if self.neg || self.is_zero() {
            // Not used on negative or zero magnitudes by the encoder.
            return self.clone();
        }
        let mut limbs = self.limbs.clone();
        let mut borrow = u64::from(subtrahend);
        for limb in &mut limbs {
            let (current, underflow) = u64::from(*limb).overflowing_sub(borrow);
            *limb = current as u32;
            borrow = underflow as u64;
            if borrow == 0 {
                break;
            }
        }
        while limbs.last() == Some(&0) {
            limbs.pop();
        }
        Self { neg: false, limbs }
    }

    /// The minimal big-endian magnitude bytes (no leading zero; empty for the value zero). This is the CBOR tag-2/3
    /// bignum payload spelling.
    pub(crate) fn to_big_endian_bytes(&self) -> alloc::vec::Vec<u8> {
        let mut groups = self.limbs.clone();
        groups.reverse();
        let mut bytes = alloc::vec::Vec::new();
        for (index, limb) in groups.iter().enumerate() {
            if index == 0 {
                // Strip leading zero bytes within the first (most significant) limb.
                let leading = limb.leading_zeros() as usize / 8;
                let significant = 4 - leading;
                for shift in (0..significant).rev() {
                    bytes.push((limb >> (shift * 8)) as u8);
                }
            } else {
                for shift in (0..4).rev() {
                    bytes.push((limb >> (shift * 8)) as u8);
                }
            }
        }
        bytes
    }

    /// Parses one canonical signed decimal spelling into the exact value.
    pub(crate) fn from_decimal_str(spelling: &str) -> Option<Self> {
        let (negative, digits) = match spelling.as_bytes().first() {
            Some(b'-') => (true, &spelling[1..]),
            Some(b'+') => (false, &spelling[1..]),
            _ => (false, spelling),
        };
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let mut value = Self::zero();
        for byte in digits.bytes() {
            let digit = u32::from(byte - b'0');
            value = value.mul_small(10).add_small(digit);
        }
        if negative {
            value = value.negated();
        }
        Some(value)
    }

    /// Raises `base` to the power `exponent` by exponentiation by squaring.
    pub(crate) fn pow(base: u32, mut exponent: u32) -> Self {
        let mut result = Self::one();
        let mut factor = Self {
            neg: false,
            limbs: vec![base],
        };
        while exponent != 0 {
            if exponent & 1 == 1 {
                result = result.mul(&factor);
            }
            exponent >>= 1;
            if exponent != 0 {
                factor = factor.mul(&factor);
            }
        }
        result
    }

    /// The canonical signed decimal text of the value.
    ///
    /// The output matches the `jqf-data` canonical integer grammar: an optional `-`, no leading zero in a longer
    /// magnitude, and `"0"` for the value zero. It never carries trailing zeroes (the exact value's canonical rendering
    /// has none), which is exactly what the tag-4 Decimal projection requires of its coefficient.
    pub(crate) fn to_decimal_string(&self) -> String {
        if self.is_zero() {
            return "0".into();
        }
        let mut magnitude = self.limbs.clone();
        let mut base10: Vec<u32> = Vec::new();
        while !magnitude.is_empty() {
            let mut remainder: u64 = 0;
            for limb in magnitude.iter_mut().rev() {
                let current = (remainder << 32) | u64::from(*limb);
                *limb = (current / u64::from(DECIMAL_BASE)) as u32;
                remainder = current % u64::from(DECIMAL_BASE);
            }
            while magnitude.last() == Some(&0) {
                magnitude.pop();
            }
            base10.push(remainder as u32);
        }
        let mut buffer = String::new();
        if self.neg {
            buffer.push('-');
        }
        // Every limb but the most significant is a fixed nine-digit base-1e9 digit, so it is zero-padded to that width.
        for (index, limb) in base10.iter().rev().enumerate() {
            let _ = if index == 0 {
                write!(buffer, "{limb}")
            } else {
                write!(buffer, "{limb:09}")
            };
        }
        buffer
    }

    /// Decomposes the exact dyadic rational `mantissa / 2^shift` into its integer floor and its unscaled decimal
    /// fraction.
    ///
    /// Returns `(whole_floor, nonzero, padded_digits)` where `whole_floor` is the mathematical floor of the magnitude,
    /// `nonzero` says whether the fraction is nonzero, and `padded_digits` is the fraction's exact decimal expansion as
    /// a fixed `shift`-digit string with leading zeroes (untouched). The value equals `whole_floor + padded_digits *
    /// 10^-shift`, so the caller can trim trailing zeroes for a positive fraction or compute the `1 - fraction`
    /// complement for a negative value without any big arithmetic.
    pub(crate) fn dyadic_parts(mantissa: u128, shift: u32) -> (u64, bool, String) {
        if shift == 0 {
            return (mantissa as u64, false, String::new());
        }
        let magnitude = u64::try_from(mantissa).unwrap_or(u64::MAX);
        let whole = if shift >= 64 { 0 } else { magnitude >> shift };
        let remainder_mask = if shift >= 64 { u64::MAX } else { (1u64 << shift) - 1 };
        let remainder = magnitude & remainder_mask;
        if remainder == 0 {
            return (whole, false, String::new());
        }
        let scaled = Self::from_u64(remainder).mul(&Self::pow(5, shift));
        let mut digits = scaled.to_decimal_string();
        while digits.len() < shift as usize {
            digits.insert(0, '0');
        }
        (whole, true, digits)
    }

    /// The exact `1 - fraction` digit string for the negative-dyadic case.
    ///
    /// `padded_digits` must be the fixed-width `shift`-digit expansion produced by [`Self::dyadic_parts`] for a nonzero
    /// fraction. The result is `(2^shift - remainder) * 5^shift` expanded to `shift` digits with trailing zeroes
    /// trimmed — computed as the digit-wise nine's-complement plus one, never by big subtraction.
    pub(crate) fn dyadic_complement(padded_digits: &str) -> String {
        let mut digits = padded_digits.as_bytes().to_vec();
        let mut carry: u32 = 1;
        for digit in digits.iter_mut().rev() {
            let nine = 9 - u32::from(*digit - b'0') + carry;
            *digit = (nine % 10) as u8 + b'0';
            carry = nine / 10;
        }
        core::str::from_utf8(&digits)
            .unwrap_or_default()
            .trim_end_matches('0')
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn big_endian_round_trip_and_decimal() {
        let b = Big::from_big_endian(&[0x01, 0x00]);
        assert_eq!(b.to_decimal_string(), "256");
        assert!(!b.is_negative());
        let zero = Big::from_big_endian(&[0x00, 0x00, 0x00]);
        assert!(zero.is_zero());
        assert_eq!(zero.to_decimal_string(), "0");
        let max = Big::from_u64(u64::MAX);
        assert_eq!(max.to_decimal_string(), "18446744073709551615");
    }

    #[test]
    fn negated_sign() {
        let b = Big::from_u64(5).negated();
        assert_eq!(b.to_decimal_string(), "-5");
        assert!(b.is_negative());
        let z = Big::zero().negated();
        assert_eq!(z.to_decimal_string(), "0");
        assert!(!z.is_negative());
    }

    #[test]
    fn multiplication() {
        let a = Big::from_u64(1_000_000_000);
        let b = Big::from_u64(1_000_000_000);
        assert_eq!(a.mul(&b).to_decimal_string(), "1000000000000000000");
        let neg = Big::from_u64(7).negated().mul(&Big::from_u64(3));
        assert_eq!(neg.to_decimal_string(), "-21");
    }

    #[test]
    fn power_of_five() {
        assert_eq!(Big::pow(5, 0).to_decimal_string(), "1");
        assert_eq!(Big::pow(5, 1).to_decimal_string(), "5");
        assert_eq!(Big::pow(5, 3).to_decimal_string(), "125");
        assert_eq!(Big::pow(5, 10).to_decimal_string(), "9765625");
    }

    #[test]
    fn dyadic_complement_cases() {
        let (_, nonzero, padded) = Big::dyadic_parts(3, 1);
        assert!(nonzero);
        assert_eq!(padded, "5");
        assert_eq!(Big::dyadic_complement(&padded), "5");
        let (_, nonzero, padded) = Big::dyadic_parts(5, 2);
        assert!(nonzero);
        assert_eq!(padded, "25");
        assert_eq!(Big::dyadic_complement(&padded), "75");
        let (_, nonzero, padded) = Big::dyadic_parts(3, 2);
        assert!(nonzero);
        assert_eq!(padded, "75");
        assert_eq!(Big::dyadic_complement(&padded), "25");
    }
}

#[cfg(test)]
mod arithmetic_tests {
    use super::*;

    #[test]
    fn big_endian_bytes_round_trip_partial_limb() {
        // A payload length not divisible by 4 must be RIGHT-aligned: a left-aligned payload scales the value by 2^(8*(4
        // - len%4)). Every length 1..=20 must round-trip exactly.
        for len in 1..=20usize {
            let mut bytes = vec![0u8; len];
            bytes[0] = 0x01;
            bytes[len - 1] = 0xab;
            let value = Big::from_big_endian(&bytes);
            assert_eq!(value.to_big_endian_bytes(), bytes, "len={len} did not round-trip");
        }
        // 2^64 decodes correctly from its 9-byte payload.
        let value = Big::from_big_endian(&[0x01, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(value.to_decimal_string(), "18446744073709551616");
    }

    #[test]
    fn big_endian_bytes_round_trip_large() {
        let value = Big::from_decimal_str("21267647932558653966388855370447585280").expect("parse");
        let bytes = value.to_big_endian_bytes();
        assert_eq!(bytes.len(), 16, "a 2^124-magnitude needs 16 bytes");
        let back = Big::from_big_endian(&bytes);
        assert_eq!(back, value);
        assert_eq!(back.to_decimal_string(), "21267647932558653966388855370447585280");
    }

    #[test]
    fn decimal_parse_round_trips() {
        for text in [
            "0",
            "1",
            "-1",
            "255",
            "256",
            "18446744073709551615",
            "-18446744073709551616",
            "21267647932558653966388855370447585280",
            "1000000000000000000000000000000000000000",
        ] {
            let value = Big::from_decimal_str(text).expect("parse");
            assert_eq!(value.to_decimal_string(), text, "round-trip {text}");
        }
    }

    #[test]
    fn add_small_carries_across_limbs() {
        let a = Big::from_u64(u64::MAX);
        assert_eq!(a.add_small(1).to_decimal_string(), "18446744073709551616");
        let a2 = Big::from_u64(0xffff_ffff).mul(&Big::from_u64(0xffff_ffff));
        // (2^32-1)^2 + 1 = 2^64 - 2^33 + 2
        assert_eq!(a2.add_small(1).to_decimal_string(), "18446744065119617026");
    }

    #[test]
    fn add_past_a_negative_value_flips_the_sign_exactly() {
        // `-3 + 5 = 2`: the addend exceeds the magnitude, so the wrapped borrow limbs must be negated back to the exact
        // small difference — not kept as the huge two's-complement residue.
        let a = Big::from_u64(3).negated();
        assert!(a.neg, "the test value must be negative");
        assert_eq!(a.add_small(5).to_decimal_string(), "2");
        // The `a <= m` side of the same branch stays negative, down to the zero boundary where the sign drops with the
        // magnitude.
        assert_eq!(a.add_small(1).to_decimal_string(), "-2");
        let zero = a.add_small(3);
        assert!(zero.is_zero());
    }

    #[test]
    fn mul_by_ten_and_small_add_keep_large_values_exact() {
        let a = Big::from_decimal_str("2126764793255865396638885537044758527").expect("a");
        assert_eq!(
            a.mul(&Big::from_u64(10)).to_decimal_string(),
            "21267647932558653966388855370447585270"
        );
        let a = Big::from_decimal_str("21267647932558653966388855370447585279").expect("a");
        assert_eq!(
            a.add_small(1).to_decimal_string(),
            "21267647932558653966388855370447585280"
        );
    }
}
