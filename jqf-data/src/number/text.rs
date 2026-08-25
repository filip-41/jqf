//! Canonical number text. Does not decide where the bytes go.
//!
//! Exact decimals use [`DecimalText`] (`0.1`, `10.250`, `1E+2`). Computed binary64 uses [`format_binary64`]: shortest
//! round-trip digits, lowercase `e`, signed two-digit exponent. Both allocate nothing.
//!
//! A NaN prints `null`. An infinity prints the widest finite binary64 (`±1.7976931348623157e+308`). The value is still
//! a number; only the spelling is special.
//!
//! This module does not parse, round, frame, or escape.

use core::fmt::{self, Write as _};

/// The longest text [`format_binary64`] can produce, with room to spare: the widest positional form is 17 significant
/// digits placed up to 15 places left of where they start, plus a sign.
const BINARY64_TEXT_CAPACITY: usize = 48;

/// The most bytes [`DecimalText`]'s synthesized prefix can need: a sign, `0.`, and up to five leading fraction zeroes
/// (`adjusted >= -6` bounds the plain form).
const HEAD_CAPACITY: usize = 16;

/// The most bytes [`DecimalText`]'s synthesized suffix can need: `E`, a sign, and the decimal digits of an `i128`
/// adjusted exponent.
const TAIL_CAPACITY: usize = 48;

/// One exact decimal's scientific-string rendering, split into the ordered byte pieces a resumable writer streams.
///
/// The coefficient digits are BORROWED (they are the only unbounded run); the sign, the `0.`-plus-zeroes prefix of a
/// pure fraction, and the `E±exponent` suffix are short and live inline. Concatenating [`Self::pieces`] in order
/// yields the exact rendering.
#[derive(Debug)]
pub struct DecimalText<'coefficient> {
    head: [u8; HEAD_CAPACITY],
    head_len: usize,
    run_a: &'coefficient [u8],
    dot: &'static [u8],
    run_b: &'coefficient [u8],
    tail: [u8; TAIL_CAPACITY],
    tail_len: usize,
}

impl<'coefficient> DecimalText<'coefficient> {
    /// Renders `coefficient * 10^-scale` in the scientific-string form.
    ///
    /// `coefficient` is an optionally `-`-prefixed run of decimal digits. Returns `None` when it carries no digits or
    /// when the exponent arithmetic leaves the representable range — both of which mean the caller was handed a value
    /// no decoder produces.
    #[must_use]
    pub fn new(coefficient: &'coefficient str, scale: i64) -> Option<Self> {
        let (negative, digits) = match coefficient.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, coefficient),
        };
        let digits = digits.as_bytes();
        let count = digits.len();
        if count == 0 {
            return None;
        }
        let exponent = -i128::from(scale);
        let adjusted = exponent + i128::try_from(count).ok()? - 1;
        // The plain positional form is used only when the value has no positive exponent and stays within six leading
        // fraction zeroes.
        let plain = exponent <= 0 && adjusted >= -6;

        let mut head = [0_u8; HEAD_CAPACITY];
        let mut head_len = 0;
        if negative {
            head[head_len] = b'-';
            head_len += 1;
        }
        let mut tail = [0_u8; TAIL_CAPACITY];
        let mut tail_len = 0;
        let (run_a, dot, run_b): (&[u8], &'static [u8], &[u8]);

        if plain {
            if exponent == 0 {
                run_a = digits;
                dot = b"";
                run_b = b"";
            } else {
                // `exponent < 0`: the point sits `-exponent` digits from the right (bounded by `count + 5` inside the
                // plain branch).
                let from_right = usize::try_from(-exponent).ok()?;
                if count > from_right {
                    let split = count - from_right;
                    run_a = digits.get(..split)?;
                    dot = b".";
                    run_b = digits.get(split..)?;
                } else {
                    head[head_len] = b'0';
                    head[head_len + 1] = b'.';
                    head_len += 2;
                    for _ in 0..(from_right - count) {
                        head[head_len] = b'0';
                        head_len += 1;
                    }
                    run_a = digits;
                    dot = b"";
                    run_b = b"";
                }
            }
        } else {
            if count == 1 {
                run_a = digits;
                dot = b"";
                run_b = b"";
            } else {
                run_a = digits.get(..1)?;
                dot = b".";
                run_b = digits.get(1..)?;
            }
            tail[tail_len] = b'E';
            tail[tail_len + 1] = if adjusted >= 0 { b'+' } else { b'-' };
            tail_len += 2;
            tail_len += write_u128_decimal(tail.get_mut(tail_len..)?, adjusted.unsigned_abs())?;
        }

        Some(Self {
            head,
            head_len,
            run_a,
            dot,
            run_b,
            tail,
            tail_len,
        })
    }

    /// The rendering's ordered byte pieces: synthesized prefix, coefficient run, optional interior point, remaining
    /// coefficient run, synthesized suffix. Some pieces are empty; their concatenation is the exact text.
    #[must_use]
    pub fn pieces(&self) -> [&[u8]; 5] {
        [
            self.head.get(..self.head_len).unwrap_or_default(),
            self.run_a,
            self.dot,
            self.run_b,
            self.tail.get(..self.tail_len).unwrap_or_default(),
        ]
    }
}

/// A stack buffer holding one rendered number, so the number path allocates nothing.
#[derive(Debug)]
pub struct Binary64Text {
    bytes: [u8; BINARY64_TEXT_CAPACITY],
    len: usize,
}

impl Default for Binary64Text {
    fn default() -> Self {
        Self {
            bytes: [0; BINARY64_TEXT_CAPACITY],
            len: 0,
        }
    }
}

impl Binary64Text {
    /// The rendered text.
    ///
    /// # Panics
    ///
    /// Panics if the rendered bytes are not valid UTF-8; the renderer only ever writes valid UTF-8 text, so the panic
    /// is unreachable in practice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).expect("rendered bytes are always valid UTF-8")
    }
}

impl fmt::Write for Binary64Text {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.len.checked_add(text.len()).ok_or(fmt::Error)?;
        self.bytes
            .get_mut(self.len..end)
            .ok_or(fmt::Error)?
            .copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// The shortest-round-trip significand digits and base-ten exponent of a finite binary64 magnitude, in one parse of
/// Rust's `{:e}` rendering.
///
/// [`format_binary64`] and [`super::Number::try_from_shortest_f64`] both need the same three facts out of
/// `{magnitude:e}` — the digits with the decimal point removed, their count, and the exponent — so the parse lives
/// here once. The caller owns the non-finite spelling law; `None` means the magnitude is not finite.
///
/// # The halfway tie
///
/// A magnitude can sit EXACTLY between the two shortest spellings that round-trip to it; both are equally short and
/// equally close, so the choice is a convention rather than a rounding. The canonical convention is the EVEN last
/// digit, where `{:e}` takes the one away from zero — so a tie is detected here ([`halfway_tie`]) and the odd
/// spelling is stepped down one.
pub(super) fn shortest_round_trip_parts(magnitude: f64) -> Option<(Binary64Text, i32)> {
    // Rust's `{:e}` yields the shortest round-tripping significand `d[.ddd]` and a base-ten exponent `x` (value =
    // significand * 10^x, no trailing zeros).
    let mut scientific = Binary64Text::default();
    write!(scientific, "{magnitude:e}").ok()?;
    let (mantissa, exponent_text) = scientific.as_str().split_once('e')?;
    let exponent: i32 = exponent_text.parse().ok()?;
    let mut digits = Binary64Text::default();
    // `{:e}` emits `d` or `d.dddd` — one significand digit, a dot only when fraction digits follow. Either way the
    // digit string is the mantissa minus its dot (which sits at index 1 when present): two contiguous writes instead of
    // a per-byte loop.
    digits.write_str(mantissa.get(..1)?).ok()?;
    if mantissa.len() > 1 {
        digits.write_str(mantissa.get(2..)?).ok()?;
    }
    let last = digits.len.checked_sub(1)?;
    let odd = digits.bytes.get(last).copied()? % 2 == 1;
    if odd && halfway_tie(magnitude, digits.len, exponent) {
        // The two tied spellings differ by one in the last digit and `{:e}` rounded up, so the even one is exactly one
        // step down. It can never borrow: an odd digit is at least 1, and a step from 1 to 0 would mean a shorter
        // spelling round-trips, which this one being shortest already denies.
        *digits.bytes.get_mut(last)? -= 1;
    }
    Some((digits, exponent))
}

/// Whether the magnitude sits EXACTLY halfway between the two spellings of `ndigits` significant digits that bracket
/// it.
///
/// No re-rendering is needed, because both sides of the question are exact. Every finite non-zero binary64 is `odd *
/// 2^shift` for exactly one `shift`, and a value with a negative `shift` has exactly `-shift` fraction digits, the last
/// of which is always a `5` (an odd multiple of `5^n` ends in `5`). The `ndigits` spelling's last digit sits at the
/// `ndigits - exponent - 1` fraction place, so the magnitude is a midpoint exactly when its own expansion runs ONE
/// place further — which is the shift test below.
fn halfway_tie(magnitude: f64, ndigits: usize, exponent: i32) -> bool {
    const SIGNIFICAND_BITS: u32 = 52;
    const EXPONENT_BIAS: i32 = 1075;

    let bits = magnitude.to_bits();
    let fraction = bits & ((1_u64 << SIGNIFICAND_BITS) - 1);
    let biased = ((bits >> SIGNIFICAND_BITS) & 0x7ff) as i32;
    let (significand, scale) = if biased == 0 {
        (fraction, 1 - EXPONENT_BIAS)
    } else {
        (fraction | (1_u64 << SIGNIFICAND_BITS), biased - EXPONENT_BIAS)
    };
    if significand == 0 {
        return false;
    }
    // `significand * 2^scale` with the significand's own factors of two moved into the scale is the unique
    // odd-times-power-of-two form.
    let Ok(shift) = i32::try_from(significand.trailing_zeros()) else {
        return false;
    };
    let Ok(ndigits) = i32::try_from(ndigits) else {
        return false;
    };
    scale + shift == exponent - ndigits
}

/// Renders one FINITE binary64 value as its shortest-round-trip string.
///
/// The point is placed positionally unless `decpt <= -4` or `decpt > ndigits + 15`, and its exponential form uses a
/// lowercase `e` with a signed, at-least-two-digit exponent — where Rust's own `{}`/`{:e}` would diverge (`1e-07`,
/// `1e+16`). A non-finite input never reaches this placement logic: it renders under the module doc's non-finite law (a
/// NaN prints the `null` literal, an infinity the clamped widest finite), never `None`.
#[must_use]
#[allow(
    clippy::missing_panics_doc,
    reason = "the digit slices are indexed within lengths derived from the same buffer"
)]
pub fn format_binary64(value: f64) -> Option<Binary64Text> {
    // The non-finite law lives in the module doc; only the spellings branch here.
    if value.is_nan() {
        let mut out = Binary64Text::default();
        out.write_str("null").ok()?;
        return Some(out);
    }
    if value.is_infinite() {
        let mut out = Binary64Text::default();
        if value.is_sign_negative() {
            out.write_char('-').ok()?;
        }
        out.write_str("1.7976931348623157e+308").ok()?;
        return Some(out);
    }
    let mut out = Binary64Text::default();
    let magnitude = value.abs();
    if value.is_sign_negative() {
        out.write_char('-').ok()?;
    }
    if magnitude == 0.0 {
        out.write_char('0').ok()?;
        return Some(out);
    }

    // Rust's `{:e}` yields the shortest round-tripping significand `d[.ddd]` and a base-ten exponent `x` (value =
    // significand * 10^x, no trailing zeros).
    let (digits, exponent) = shortest_round_trip_parts(magnitude)?;
    let digits = digits.as_str();
    let ndigits = i32::try_from(digits.len()).ok()?;
    // decpt: the count of digits before the decimal point in positional form.
    let decpt = exponent + 1;

    if decpt <= -4 || decpt > ndigits + 15 {
        out.write_str(digits.get(..1)?).ok()?;
        if digits.len() > 1 {
            out.write_char('.').ok()?;
            out.write_str(digits.get(1..)?).ok()?;
        }
        out.write_char('e').ok()?;
        out.write_char(if exponent >= 0 { '+' } else { '-' }).ok()?;
        let exponent_magnitude = exponent.unsigned_abs();
        if exponent_magnitude < 10 {
            out.write_char('0').ok()?;
        }
        write!(out, "{exponent_magnitude}").ok()?;
    } else if decpt <= 0 {
        out.write_str("0.").ok()?;
        for _ in 0..(-decpt) {
            out.write_char('0').ok()?;
        }
        out.write_str(digits).ok()?;
    } else if decpt >= ndigits {
        out.write_str(digits).ok()?;
        for _ in 0..(decpt - ndigits) {
            out.write_char('0').ok()?;
        }
    } else {
        let split = usize::try_from(decpt).ok()?;
        out.write_str(digits.get(..split)?).ok()?;
        out.write_char('.').ok()?;
        out.write_str(digits.get(split..)?).ok()?;
    }
    Some(out)
}

/// Writes `value`'s decimal digits into `out`, returning how many bytes it used, or `None` when `out` is shorter than
/// the spelling — a caller bug answered rather than expected away.
pub(super) fn write_u128_decimal(out: &mut [u8], value: u128) -> Option<usize> {
    let mut scratch = [0_u8; 40];
    let mut cursor = scratch.len();
    let mut magnitude = value;
    loop {
        cursor -= 1;
        // A single decimal digit always fits `u8`.
        scratch[cursor] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    let written = scratch.get(cursor..)?;
    let target = out.get_mut(..written.len())?;
    target.copy_from_slice(written);
    Some(written.len())
}

#[cfg(test)]
mod tests {
    use super::{DecimalText, format_binary64};
    use alloc::string::String;

    fn decimal(coefficient: &str, scale: i64) -> String {
        let text = DecimalText::new(coefficient, scale).expect("renderable decimal");
        let mut out = String::new();
        for piece in text.pieces() {
            out.push_str(core::str::from_utf8(piece).expect("ASCII piece"));
        }
        out
    }

    #[test]
    fn decimal_matches_the_scientific_string_rendering() {
        // The expected spellings are the canonical rendering of the same literals.
        assert_eq!(decimal("1", 0), "1");
        assert_eq!(decimal("15", 1), "1.5");
        assert_eq!(decimal("-15", 1), "-1.5");
        assert_eq!(decimal("1", 1), "0.1");
        assert_eq!(decimal("10250", 3), "10.250");
        assert_eq!(decimal("10", 1), "1.0");
        assert_eq!(decimal("0", 2), "0.00");
        assert_eq!(decimal("1", -2), "1E+2");
        assert_eq!(decimal("1", -308), "1E+308");
        assert_eq!(decimal("1", 323), "1E-323");
        assert_eq!(decimal("1", 7), "1E-7");
        assert_eq!(decimal("1", 6), "0.000001");
    }

    /// The renderer's two refusal classes, and the boundary that is NOT one: a coefficient carrying no digits declines,
    /// while a scale at either end of `i64` still renders — the doc's "exponent arithmetic leaves the representable
    /// range" is about the head/tail buffers, not about large scales, and the rows below keep the two from being
    /// confused.
    #[test]
    fn a_digitless_coefficient_declines_where_an_extreme_scale_renders() {
        assert!(DecimalText::new("", 0).is_none());
        assert!(DecimalText::new("-", 0).is_none());
        assert_eq!(decimal("1", i64::MIN), "1E+9223372036854775808");
        assert_eq!(decimal("-1", i64::MAX), "-1E-9223372036854775807");
    }

    #[test]
    #[expect(
        clippy::unreadable_literal,
        reason = "the literal IS the expected rendering; separators would obscure the byte-for-byte correspondence"
    )]
    fn binary64_matches_the_canonical_placement() {
        let render = |value: f64| String::from(format_binary64(value).expect("finite").as_str());
        assert_eq!(render(0.0), "0");
        assert_eq!(render(-0.0), "-0");
        assert_eq!(render(1.5), "1.5");
        assert_eq!(render(1.0), "1");
        assert_eq!(render(0.30000000000000004), "0.30000000000000004");
        assert_eq!(render(1e-7), "1e-07");
        assert_eq!(render(1e-5), "1e-05");
        assert_eq!(render(1e-4), "0.0001");
        assert_eq!(render(1e15), "1000000000000000");
        assert_eq!(render(1e16), "1e+16");
        assert_eq!(render(123_456.789), "123456.789");
        assert_eq!(render(f64::MAX), "1.7976931348623157e+308");
        assert_eq!(render(1.0 / 3.0), "0.3333333333333333");
    }

    /// The halfway tie takes the EVEN last digit.
    ///
    /// `10979717819269.0625` is the row that bites: it is exactly midway between `…269.062` and `…269.063`, both of
    /// which round-trip and both of which are the shortest that do, so nothing about round-tripping decides between
    /// them — only the even-digit convention does, and the platform's own shortest rendering takes the other one
    /// (away from zero). The second row pins a tie one place further left (`…482.25`), so the detection is not
    /// reading a fixed digit position. The last two rows are the guard against over-firing: the immediate ULP
    /// neighbours of a tie are NOT ties, and their nearest spellings stand untouched even though a second-shortest
    /// spelling still round-trips there. The odd last digits that must survive across the ordinary range are
    /// [`Self::binary64_matches_the_canonical_placement`]'s rows.
    #[test]
    #[expect(
        clippy::unreadable_literal,
        clippy::excessive_precision,
        reason = "the literals are the EXACT tied values; separators or a shortened spelling would hide that each sits precisely on a midpoint, which is the whole of what the test pins"
    )]
    fn binary64_halfway_ties_take_the_even_digit() {
        let render = |value: f64| String::from(format_binary64(value).expect("finite").as_str());
        assert_eq!(render(10979717819269.0625), "10979717819269.062");
        assert_eq!(render(-10979717819269.0625), "-10979717819269.062");
        assert_eq!(render(800292641346482.25), "800292641346482.2");
        // Not ties: the shortest spelling is the nearest one, untouched.
        assert_eq!(
            render(f64::from_bits(10979717819269.0625_f64.to_bits() + 1)),
            "10979717819269.064"
        );
        assert_eq!(
            render(f64::from_bits(10979717819269.0625_f64.to_bits() - 1)),
            "10979717819269.06"
        );
    }

    #[test]
    fn non_finite_binary64_renders_null_or_clamped() {
        // Pins the module doc's non-finite law: a NaN renders as the `null` literal, and either infinity clamps to the
        // widest finite binary64's shortest text. The value stays a `number`; only the bytes are clamped or nulled.
        assert_eq!(format_binary64(f64::NAN).expect("nan renders").as_str(), "null");
        assert_eq!(
            format_binary64(f64::INFINITY).expect("infinity renders").as_str(),
            "1.7976931348623157e+308"
        );
        assert_eq!(
            format_binary64(f64::NEG_INFINITY)
                .expect("negative infinity renders")
                .as_str(),
            "-1.7976931348623157e+308"
        );
    }
}
