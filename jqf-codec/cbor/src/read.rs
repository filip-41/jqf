//! Low-level RFC 8949 byte reader.
//!
//! This module owns the initial-byte dispatch, the big-endian argument decoding, the BREAK byte, and the exact binary64
//! widening of f16/f32 that preserves NaN sign/significand bits (never a host cast — the sign is kept, the binary64
//! exponent is all ones, and the source significand is right-zero-extended with `f16_fraction << 42` or `f32_fraction
//! << 29`). The container-stack grammar and document building live in [`crate::parse`]; this module knows nothing about
//! documents.
//!
//! The initial-byte row comes from a pure const fn ([`dispatch`]), leaving the compiler free to lower the match to a
//! jump table.

/// One major type, per RFC 8949 §3.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Major {
    /// Major type 0: unsigned integer.
    UInt,
    /// Major type 1: negative integer.
    NegInt,
    /// Major type 2: byte string.
    Bytes,
    /// Major type 3: text string.
    Text,
    /// Major type 4: array.
    Array,
    /// Major type 5: map.
    Map,
    /// Major type 6: tag.
    Tag,
    /// Major type 7: simple values and floats.
    Simple,
}

/// How the additional-information bits select the argument.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ArgKind {
    /// Argument is the additional-information value itself (0..=23).
    Direct,
    /// One following big-endian byte.
    One,
    /// Two following big-endian bytes.
    Two,
    /// Four following big-endian bytes.
    Four,
    /// Eight following big-endian bytes.
    Eight,
    /// Indefinite length (additional information 31).
    Indef,
    /// BREAK byte, only legal inside an indefinite container.
    Break,
    /// One following byte naming a simple value.
    Simple,
    /// Half-precision float, two following bytes.
    Float16,
    /// Single-precision float, four following bytes.
    Float32,
    /// Double-precision float, eight following bytes.
    Float64,
    /// Reserved additional-information or simple value (28..=30, two-byte simple < 32).
    Reserved,
}

/// One initial-byte dispatch row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Dispatch {
    /// The item's major type.
    pub(crate) major: Major,
    /// How the argument is selected.
    pub(crate) arg: ArgKind,
}

/// The argument of a decoded item head.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Arg {
    /// A definite unsigned argument.
    UInt(u64),
    /// Indefinite length.
    Indef,
}

/// A decoded item head: major type plus argument.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Head {
    /// The item's major type.
    pub(crate) major: Major,
    /// The decoded argument.
    pub(crate) arg: Arg,
}

/// A decode failure with the byte offset at which it was detected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReadError {
    /// The item cannot be read: too few bytes remain.
    Eof,
    /// Reserved additional-information value 28..=30.
    ReservedArgument,
    /// A two-byte simple value below 32 (RFC 8949 §3.3: well-formed only for ≥ 32).
    ReservedSimple,
}

/// Decodes the initial byte into its dispatch row.
#[must_use]
pub(crate) const fn dispatch(initial: u8) -> Dispatch {
    let major = match initial >> 5 {
        0 => Major::UInt,
        1 => Major::NegInt,
        2 => Major::Bytes,
        3 => Major::Text,
        4 => Major::Array,
        5 => Major::Map,
        6 => Major::Tag,
        _ => Major::Simple,
    };
    let additional = initial & 0x1f;
    let arg = if matches!(major, Major::Simple) {
        match additional {
            24 => ArgKind::Simple,
            25 => ArgKind::Float16,
            26 => ArgKind::Float32,
            27 => ArgKind::Float64,
            31 => ArgKind::Break,
            value if value <= 23 => ArgKind::Direct,
            _ => ArgKind::Reserved,
        }
    } else {
        match additional {
            value if value <= 23 => ArgKind::Direct,
            24 => ArgKind::One,
            25 => ArgKind::Two,
            26 => ArgKind::Four,
            27 => ArgKind::Eight,
            31 => ArgKind::Indef,
            _ => ArgKind::Reserved,
        }
    };
    Dispatch { major, arg }
}

/// Reads the big-endian unsigned value in `bytes[pos..]`.
///
/// Returns the value and the position just past it. Callers pre-check the bounds: `Eof` is produced by [`head`] with
/// `checked` arithmetic before any of these run.
#[must_use]
pub(crate) fn read_u64(bytes: &[u8], pos: usize) -> u64 {
    debug_assert!(bytes.len().saturating_sub(pos) >= 8);
    let chunk = <&[u8; 8]>::try_from(&bytes[pos..pos + 8]).expect("length checked");
    u64::from_be_bytes(*chunk)
}

/// Reads the big-endian unsigned value in `bytes[pos..]`.
#[must_use]
pub(crate) fn read_u32(bytes: &[u8], pos: usize) -> u32 {
    debug_assert!(bytes.len().saturating_sub(pos) >= 4);
    let chunk = <&[u8; 4]>::try_from(&bytes[pos..pos + 4]).expect("length checked");
    u32::from_be_bytes(*chunk)
}

/// Reads the big-endian unsigned value in `bytes[pos..]`.
#[must_use]
pub(crate) fn read_u16(bytes: &[u8], pos: usize) -> u16 {
    debug_assert!(bytes.len().saturating_sub(pos) >= 2);
    let chunk = <&[u8; 2]>::try_from(&bytes[pos..pos + 2]).expect("length checked");
    u16::from_be_bytes(*chunk)
}

/// Decodes one item head at `pos`, returning it and the position just past the head (arguments included, payload bytes
/// excluded).
///
/// A major-type-7 float/simple row is folded into a `Head` whose `arg` is the additional-information value, except that
/// the one-byte simple form is folded into the `UInt` argument when legal. Simple values 20..=23
/// (`false`/`true`/`null`/`undefined`) dispatch as `Direct` and fold through the generic arm, which yields exactly
/// `Arg::UInt(20..23)`.
#[allow(
    clippy::match_same_arms,
    reason = "the major-type-7 BREAK row and the generic INDEF row fold to the same Arg but are distinct dispatch combinations"
)]
pub(crate) fn head(bytes: &[u8], pos: usize) -> Result<(Head, usize), ReadError> {
    let Some(&initial) = bytes.get(pos) else {
        return Err(ReadError::Eof);
    };
    let row = dispatch(initial);
    let head_end = |additional: usize| -> Result<usize, ReadError> {
        let end = pos
            .checked_add(1 + additional)
            .filter(|&end| end <= bytes.len())
            .ok_or(ReadError::Eof)?;
        Ok(end)
    };
    let (arg, end) = match (row.major, row.arg) {
        (Major::Simple, ArgKind::Simple) => {
            let &value = bytes.get(pos + 1).ok_or(ReadError::Eof)?;
            if value < 32 {
                return Err(ReadError::ReservedSimple);
            }
            (Arg::UInt(u64::from(value)), 2)
        }
        // The float rows fold their additional-information value into the arg; the parse layer re-reads the payload
        // bytes by that width. The end goes through the same bounds filter as every argument row, so a truncated
        // payload at end-of-buffer is Eof, not an out-of-range span.
        (Major::Simple, ArgKind::Float16) => {
            let end = head_end(2)?;
            (Arg::UInt(25), end - pos)
        }
        (Major::Simple, ArgKind::Float32) => {
            let end = head_end(4)?;
            (Arg::UInt(26), end - pos)
        }
        (Major::Simple, ArgKind::Float64) => {
            let end = head_end(8)?;
            (Arg::UInt(27), end - pos)
        }
        // Simple values 0..=23, the BREAK byte, and the reserved rows share their bodies with the generic catch-alls
        // below; BREAK and INDEF are DISTINCT `ArgKind`s that happen to fold to the same `Arg::Indef`.
        (Major::Simple, ArgKind::Break) => (Arg::Indef, 1),
        (_, ArgKind::Direct) => (Arg::UInt(u64::from(initial & 0x1f)), 1),
        (_, ArgKind::Indef) => (Arg::Indef, 1),
        (_, ArgKind::Reserved) => return Err(ReadError::ReservedArgument),
        (_, ArgKind::One) => {
            let &value = bytes.get(pos + 1).ok_or(ReadError::Eof)?;
            (Arg::UInt(u64::from(value)), 2)
        }
        (_, ArgKind::Two) => {
            let end = head_end(2)?;
            (Arg::UInt(u64::from(read_u16(bytes, pos + 1))), end - pos)
        }
        (_, ArgKind::Four) => {
            let end = head_end(4)?;
            (Arg::UInt(u64::from(read_u32(bytes, pos + 1))), end - pos)
        }
        (_, ArgKind::Eight) => {
            let end = head_end(8)?;
            (Arg::UInt(read_u64(bytes, pos + 1)), end - pos)
        }
        // Simple-value and float rows are handled by the major-type-7 arms above; any other combination cannot arise
        // from `dispatch`, and a head that reached here is not one this reader can spell.
        (_, _) => return Err(ReadError::ReservedArgument),
    };
    let end = pos.checked_add(end).ok_or(ReadError::Eof)?;
    Ok((Head { major: row.major, arg }, end))
}

/// Writes the exact binary64 bits for a half-precision payload, preserving NaN sign/significand and signed zero per the
/// projection law.
#[must_use]
pub(crate) fn widen_f16(bits: u16) -> u64 {
    let sign = u64::from(bits >> 15) << 63;
    let exp = u64::from((bits >> 10) & 0x1f);
    let frac = u64::from(bits & 0x3ff);
    match exp {
        0x1f => sign | (0x7ff << 52) | (frac << 42),
        0 => {
            if frac == 0 {
                sign
            } else {
                // Subnormal: value = frac * 2^-24. Normalize the 10-bit significand to a normal binary64: the highest
                // set bit of the widened significand lands at bit 52 (the implicit one), the stored fraction is the
                // remainder below it, and the exponent field is `p + 999`.
                let p = frac.ilog2(); // highest set bit (0..=9)
                let normalized = (frac << (52 - p)) & ((1u64 << 52) - 1);
                sign | ((u64::from(p) + 999) << 52) | normalized
            }
        }
        exp => sign | ((exp + 1008) << 52) | (frac << 42),
    }
}

/// Narrows an exact binary64 value to the nearest half-precision payload, then widens it back; `Some` only when the
/// round-trip is EXACT (the value is exactly representable in f16). This is the encoder's "shortest float width that
/// reconstructs exact value" check. NaN is never narrowed here: the caller owns the profile's NaN rule.
#[must_use]
pub(crate) fn narrow_f16(bits: u64) -> Option<u16> {
    let candidate = round_f16(bits);
    if widen_f16(candidate) == bits {
        Some(candidate)
    } else {
        None
    }
}

/// The nearest half-precision payload for an exact binary64 value (round to nearest, ties to even), including the
/// subnormal range. Infinities fold to the half-precision infinities; a NaN returns its top significand bits as the
/// quiet form, though the caller owns the profile's NaN rule and gates every other NaN out before narrowing.
fn round_f16(bits: u64) -> u16 {
    let sign = u16::try_from((bits >> 63) & 1).unwrap_or(0);
    let sign16 = sign << 15;
    let exp = (bits >> 52) & 0x7ff;
    let frac = bits & 0x000f_ffff_ffff_ffff;
    if exp == 0x7ff {
        if frac == 0 {
            return sign16 | 0x7c00;
        }
        let payload = u16::try_from(frac >> 42).unwrap_or(0);
        return sign16 | 0x7e00 | payload;
    }
    if exp == 0 {
        return sign16;
    }
    let e16 = i64::try_from(exp).unwrap_or(0) - 1008;
    let sig = (1u64 << 52) | frac;
    if (1..=30).contains(&e16) {
        let base = sig >> 42;
        let disc = sig & ((1u64 << 42) - 1);
        let rounded = base + u64::from(disc > 1 << 41 || (disc == 1 << 41 && (base & 1) == 1));
        if rounded >= 1 << 11 {
            return sign16 | (u16::try_from(e16 + 1).unwrap_or(0) << 10);
        }
        return sign16 | (u16::try_from(e16).unwrap_or(0) << 10) | u16::try_from(rounded & 0x3ff).unwrap_or(0);
    }
    // Subnormal range (value < 2^-14, down to 2^-24): `frac16 = round(sig * 2^(exp - 1051))`.
    let shift = 1051 - i64::try_from(exp).unwrap_or(0);
    if shift <= 0 || shift >= 64 {
        return sign16;
    }
    let shift = u32::try_from(shift).unwrap_or(63);
    let base = sig >> shift;
    let disc = sig & ((1u64 << shift) - 1);
    let rounded = base + u64::from(disc > 1 << (shift - 1) || (disc == 1 << (shift - 1) && (base & 1) == 1));
    if rounded == 0 {
        return sign16;
    }
    if rounded >= 0x400 {
        return sign16 | (1 << 10);
    }
    sign16 | u16::try_from(rounded).unwrap_or(0)
}

/// Narrows an exact binary64 value to the nearest single-precision payload via the host's exact f64-to-f32 rounding,
/// `Some` only when widening back reproduces the exact bits. NaN is never narrowed here.
#[must_use]
pub(crate) fn narrow_f32(bits: u64) -> Option<u32> {
    let value = f64::from_bits(bits);
    let narrowed = value as f32;
    if f64::from(narrowed).to_bits() == bits {
        Some(narrowed.to_bits())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{Arg, ArgKind, Dispatch, Head, Major, ReadError, dispatch, head, widen_f16};

    #[test]
    fn dispatch_covers_the_six_non_simple_majors() {
        assert_eq!(
            dispatch(0x00),
            Dispatch {
                major: Major::UInt,
                arg: ArgKind::Direct
            }
        );
        assert_eq!(
            dispatch(0x20),
            Dispatch {
                major: Major::NegInt,
                arg: ArgKind::Direct
            }
        );
        assert_eq!(
            dispatch(0x40),
            Dispatch {
                major: Major::Bytes,
                arg: ArgKind::Direct
            }
        );
        assert_eq!(
            dispatch(0x60),
            Dispatch {
                major: Major::Text,
                arg: ArgKind::Direct
            }
        );
        assert_eq!(
            dispatch(0x80),
            Dispatch {
                major: Major::Array,
                arg: ArgKind::Direct
            }
        );
        assert_eq!(
            dispatch(0xa0),
            Dispatch {
                major: Major::Map,
                arg: ArgKind::Direct
            }
        );
        assert_eq!(
            dispatch(0xc0),
            Dispatch {
                major: Major::Tag,
                arg: ArgKind::Direct
            }
        );
        assert_eq!(
            dispatch(0xe0),
            Dispatch {
                major: Major::Simple,
                arg: ArgKind::Direct
            }
        );
    }

    #[test]
    fn simple_rows_dispatch_to_their_kinds() {
        // Simple values 20..=23 (false/true/null/undefined) fold through the generic Direct row; the `head` test below
        // pins the identical `Arg::UInt(20..23)` outcomes.
        assert_eq!(
            dispatch(0xf4),
            Dispatch {
                major: Major::Simple,
                arg: ArgKind::Direct
            }
        );
        assert_eq!(
            dispatch(0xf5),
            Dispatch {
                major: Major::Simple,
                arg: ArgKind::Direct
            }
        );
        assert_eq!(
            dispatch(0xf6),
            Dispatch {
                major: Major::Simple,
                arg: ArgKind::Direct
            }
        );
        assert_eq!(
            dispatch(0xf7),
            Dispatch {
                major: Major::Simple,
                arg: ArgKind::Direct
            }
        );
        assert_eq!(
            dispatch(0xf8),
            Dispatch {
                major: Major::Simple,
                arg: ArgKind::Simple
            }
        );
        assert_eq!(
            dispatch(0xf9),
            Dispatch {
                major: Major::Simple,
                arg: ArgKind::Float16
            }
        );
        assert_eq!(
            dispatch(0xfa),
            Dispatch {
                major: Major::Simple,
                arg: ArgKind::Float32
            }
        );
        assert_eq!(
            dispatch(0xfb),
            Dispatch {
                major: Major::Simple,
                arg: ArgKind::Float64
            }
        );
        assert_eq!(
            dispatch(0xff),
            Dispatch {
                major: Major::Simple,
                arg: ArgKind::Break
            }
        );
        // Reserved additional-information values.
        assert_eq!(
            dispatch(0x1c),
            Dispatch {
                major: Major::UInt,
                arg: ArgKind::Reserved
            }
        );
        assert_eq!(
            dispatch(0xfc),
            Dispatch {
                major: Major::Simple,
                arg: ArgKind::Reserved
            }
        );
    }

    #[test]
    fn head_decodes_direct_and_extended_arguments() {
        assert_eq!(
            head(&[0x17], 0),
            Ok((
                Head {
                    major: Major::UInt,
                    arg: Arg::UInt(23)
                },
                1
            ))
        );
        assert_eq!(
            head(&[0x18, 0xff], 0),
            Ok((
                Head {
                    major: Major::UInt,
                    arg: Arg::UInt(255)
                },
                2
            ))
        );
        assert_eq!(
            head(&[0x19, 0x12, 0x34], 0),
            Ok((
                Head {
                    major: Major::UInt,
                    arg: Arg::UInt(0x1234)
                },
                3
            ))
        );
        assert_eq!(
            head(&[0x1a, 0x01, 0x02, 0x03, 0x04], 0),
            Ok((
                Head {
                    major: Major::UInt,
                    arg: Arg::UInt(0x0102_0304)
                },
                5
            ))
        );
        assert_eq!(
            head(&[0x1b, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08], 0),
            Ok((
                Head {
                    major: Major::UInt,
                    arg: Arg::UInt(0x0102_0304_0506_0708)
                },
                9
            ))
        );
        assert_eq!(
            head(&[0x9f], 0),
            Ok((
                Head {
                    major: Major::Array,
                    arg: Arg::Indef
                },
                1
            ))
        );
    }

    #[test]
    fn head_rejects_reserved_and_truncated_input() {
        assert_eq!(head(&[0x1c], 0), Err(ReadError::ReservedArgument));
        assert_eq!(head(&[0x1d], 0), Err(ReadError::ReservedArgument));
        assert_eq!(head(&[0x1e], 0), Err(ReadError::ReservedArgument));
        assert_eq!(
            head(&[0x1f], 0),
            Ok((
                Head {
                    major: Major::UInt,
                    arg: Arg::Indef
                },
                1
            ))
        );
        assert_eq!(head(&[], 0), Err(ReadError::Eof));
        assert_eq!(head(&[0x19, 0x12], 0), Err(ReadError::Eof));
        assert_eq!(head(&[0x18], 0), Err(ReadError::Eof));
        assert_eq!(head(&[0xf8, 0x00], 0), Err(ReadError::ReservedSimple));
        assert_eq!(head(&[0xf8, 0x14], 0), Err(ReadError::ReservedSimple));
        assert_eq!(head(&[0xf8, 0x17], 0), Err(ReadError::ReservedSimple));
        assert_eq!(head(&[0xf8, 0x1c], 0), Err(ReadError::ReservedSimple));
        assert_eq!(head(&[0xf8, 0x1f], 0), Err(ReadError::ReservedSimple));
        // The two-byte form is well-formed only for value ≥ 32.
        assert_eq!(
            head(&[0xf8, 0x20], 0),
            Ok((
                Head {
                    major: Major::Simple,
                    arg: Arg::UInt(32)
                },
                2
            ))
        );
    }

    #[test]
    fn simple_value_heads_fold_to_arguments() {
        assert_eq!(
            head(&[0xf4], 0),
            Ok((
                Head {
                    major: Major::Simple,
                    arg: Arg::UInt(20)
                },
                1
            ))
        );
        assert_eq!(
            head(&[0xf5], 0),
            Ok((
                Head {
                    major: Major::Simple,
                    arg: Arg::UInt(21)
                },
                1
            ))
        );
        assert_eq!(
            head(&[0xf6], 0),
            Ok((
                Head {
                    major: Major::Simple,
                    arg: Arg::UInt(22)
                },
                1
            ))
        );
        assert_eq!(
            head(&[0xf7], 0),
            Ok((
                Head {
                    major: Major::Simple,
                    arg: Arg::UInt(23)
                },
                1
            ))
        );
        assert_eq!(
            head(&[0xe1], 0),
            Ok((
                Head {
                    major: Major::Simple,
                    arg: Arg::UInt(1)
                },
                1
            ))
        );
    }

    #[test]
    fn f16_widening_is_exact_and_preserves_bits() {
        // +1.0 in f16 (0x3c00) widens to the binary64 bit pattern of 1.0.
        assert_eq!(widen_f16(0x3c00), 0x3ff0_0000_0000_0000);
        // -0.0 preserves the sign bit.
        assert_eq!(widen_f16(0x8000), 0x8000_0000_0000_0000);
        // +0.0.
        assert_eq!(widen_f16(0x0000), 0x0000_0000_0000_0000);
        // +inf -> 0x7ff0...
        assert_eq!(widen_f16(0x7c00), 0x7ff0_0000_0000_0000);
        // -inf.
        assert_eq!(widen_f16(0xfc00), 0xfff0_0000_0000_0000);
        // NaN with payload 0x215: fraction << 42.
        assert_eq!(widen_f16(0x7e15), 0x7ff8_5400_0000_0000);
        // Subnormal f16 0x0001 (2^-24) widens to the smallest positive normal binary64 with exponent field 999 (value
        // 2^-24).
        assert_eq!(widen_f16(0x0001), 0x3e70_0000_0000_0000);
        // 0.5 in f16 (0x3800).
        assert_eq!(widen_f16(0x3800), 0x3fe0_0000_0000_0000);
    }
}
