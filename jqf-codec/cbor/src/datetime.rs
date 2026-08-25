//! Tag-0 RFC 3339 parsing and tag-1 epoch projection.
//!
//! The generic dialect validates tag 0 against the RFC 3339 `date-time` production refined by RFC 4287 section 3.3
//! (uppercase `T`/`Z`) and tag 1 by the proleptic POSIX law from the portfolio: every civil day is exactly 86,400
//! seconds, negative fractions use mathematical floor across the prior second, and no leap-second label is invented.
//! Both project only when the instant and fraction are exactly representable in the core temporal types and year range;
//! any deviation returns `None` and the caller retains the exact tagged payload instead.
//!
//! The RFC 3339 parser now lives in `jqf_data::temporal::parse_rfc3339` — the one implementation across the tree.
//! This module delegates to it and translates its `TemporalError` to the codec-local `Rfc3339Error`.

use alloc::string::String;
use jqf_data::{
    FractionalSecond, KnownUtcOffset, LocalDateTime, LocalTime, OffsetDateTime, UtcOffset, civil_from_epoch,
};

use crate::big::Big;

/// The first representable second: `0000-01-01T00:00:00Z`, derived from the same civil-calendar law `jqf_data` enforces
/// (`civil_from_epoch` rejects years outside `0000..=9999`) rather than a hand-copied constant, so the two cannot drift
/// apart silently.
const MIN_SECONDS: i64 = jqf_data::days_from_civil(0, 1, 1) * 86_400;
/// The last representable second: `9999-12-31T23:59:59Z`.
const MAX_SECONDS: i64 = (jqf_data::days_from_civil(9_999, 12, 31) + 1) * 86_400 - 1;

/// Why an RFC 3339 tag-0 payload cannot project.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Rfc3339Error {
    /// The payload does not satisfy the RFC 3339 `date-time` production or its field ranges (a generic-validation
    /// failure).
    Syntax,
    /// The payload is valid RFC 3339 but its year exceeds the core year range `0000..=9999` (the exact tagged payload
    /// is retained).
    OutOfRange,
}

/// Parses one RFC 3339 `date-time` (uppercase `T`/`Z`, RFC 4287 refinement).
///
/// Delegates to the shared `jqf_data::parse_rfc3339` parser. RFC 3339 §5.6 permits lowercase `t`/`z`, and the shared
/// parser refuses them; that narrowing is exactly this codec's own tag-0 refinement (RFC 4287 §3.3 mandates
/// uppercase), so delegating costs nothing here. The refusal is pinned by the lowercase row in
/// `rfc3339_fraction_and_offsets` below.
pub(crate) fn parse_rfc3339(text: &str) -> Result<OffsetDateTime, Rfc3339Error> {
    jqf_data::parse_rfc3339(text).map_err(|error| match error {
        jqf_data::TemporalError::OutOfRange => Rfc3339Error::OutOfRange,
        _ => Rfc3339Error::Syntax,
    })
}

/// Projects a tag-1 integer count of POSIX seconds to a zero-offset date-time, or `None` when the instant falls outside
/// the core year range.
pub(crate) fn epoch_integer_to_offset(seconds: i64) -> Option<OffsetDateTime> {
    if !(MIN_SECONDS..=MAX_SECONDS).contains(&seconds) {
        return None;
    }
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400) as u32;
    let date = civil_from_epoch(days)?;
    let time = LocalTime::new(
        (second_of_day / 3600) as u8,
        ((second_of_day % 3600) / 60) as u8,
        (second_of_day % 60) as u8,
        FractionalSecond::parse("").ok()?,
    )?;
    Some(OffsetDateTime {
        local: LocalDateTime { date, time },
        offset: UtcOffset::KnownSeconds(KnownUtcOffset::new(0)?),
    })
}

/// Projects a tag-1 float count of POSIX seconds to a zero-offset date-time.
///
/// The exact dyadic value is decomposed: the mathematical floor supplies the seconds (POSIX negative-fraction law) and
/// the exact decimal expansion of the remainder supplies the fraction. `None` covers non-finite values, out-of-range
/// instants, and the `-0.0`/`+0.0` integral edge (returned as a valid epoch zero, not a failure).
pub(crate) fn epoch_float_to_offset(value: f64) -> Option<OffsetDateTime> {
    if !value.is_finite() {
        return None;
    }
    let bits = value.to_bits();
    let negative = bits >> 63 != 0;
    let exponent = ((bits >> 52) & 0x7ff) as i32;
    let mantissa = bits & 0x000f_ffff_ffff_ffff;
    let (significand, shift) = if exponent == 0 {
        (mantissa, 1074i32)
    } else {
        (mantissa | (1 << 52), 1075i32 - exponent)
    };
    if shift <= 0 {
        // Integral value. The year-range bound guarantees |value| < 2^53, so the cast is exact.
        let seconds = value as i64;
        if !(MIN_SECONDS..=MAX_SECONDS).contains(&seconds) {
            return None;
        }
        return epoch_integer_to_offset(seconds);
    }
    let (whole, nonzero, padded) = Big::dyadic_parts(significand as u128, shift as u32);
    let seconds = if !negative {
        i64::try_from(whole).ok()?
    } else if nonzero {
        -(whole as i64).checked_add(1)?
    } else {
        -(whole as i64)
    };
    if !(MIN_SECONDS..=MAX_SECONDS).contains(&seconds) {
        return None;
    }
    let fraction = if !nonzero {
        String::new()
    } else if negative {
        Big::dyadic_complement(&padded)
    } else {
        padded.trim_end_matches('0').into()
    };
    // The floor seconds determine the civil date and clock; only the fractional second is replaced with the exact
    // dyadic remainder.
    let mut result = epoch_integer_to_offset(seconds)?;
    result.local.time = LocalTime::new(
        result.local.time.hour(),
        result.local.time.minute(),
        result.local.time.second(),
        FractionalSecond::parse(&fraction).ok()?,
    )?;
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_epoch_known_values() {
        let value = parse_rfc3339("1970-01-01T00:00:00Z").expect("valid epoch date-time");
        assert_eq!(value.local.date.year(), 1970);
        assert_eq!(value.local.date.month(), 1);
        assert_eq!(value.local.date.day(), 1);
        assert_eq!(value.local.time.hour(), 0);
        assert!(matches!(value.offset, UtcOffset::KnownSeconds(_)));
    }

    #[test]
    fn rfc3339_fraction_and_offsets() {
        let value = parse_rfc3339("2024-06-01T12:34:56.123Z").expect("valid fraction");
        assert_eq!(value.local.time.fraction().digits(), "123");
        let negative = parse_rfc3339("2024-06-01T12:34:56-05:30").expect("valid offset");
        assert!(matches!(negative.offset, UtcOffset::KnownSeconds(_)));
        let unknown = parse_rfc3339("2024-06-01T12:34:56-00:00").expect("unknown-local offset");
        assert!(matches!(unknown.offset, UtcOffset::UnknownLocalOffset));
        assert!(matches!(
            parse_rfc3339("2024-06-01t12:34:56z"),
            Err(Rfc3339Error::Syntax)
        ));
        assert!(matches!(
            parse_rfc3339("2024-06-01 12:34:56Z"),
            Err(Rfc3339Error::Syntax)
        ));
        assert!(matches!(
            parse_rfc3339("2024-13-01T12:34:56Z"),
            Err(Rfc3339Error::Syntax)
        ));
        // RFC 3339 years are exactly four digits, so a five-digit year is a syntax error, not an out-of-core-range
        // value.
        assert!(matches!(
            parse_rfc3339("10000-01-01T00:00:00Z"),
            Err(Rfc3339Error::Syntax)
        ));
    }

    #[test]
    fn rfc3339_offset_minutes_are_scaled_to_seconds() {
        // `+04:02` scales to 4*3600 + 2*60 = 14520 seconds — every minute term carries its `* 60`, not just the hour
        // term.
        let value = parse_rfc3339("2024-10-15T14:35:31+04:02").expect("valid offset");
        let UtcOffset::KnownSeconds(offset) = value.offset else {
            panic!("expected a known offset");
        };
        assert_eq!(offset.seconds(), 4 * 3600 + 2 * 60);

        let negative = parse_rfc3339("2024-10-15T14:35:31-04:02").expect("valid offset");
        let UtcOffset::KnownSeconds(negative_offset) = negative.offset else {
            panic!("expected a known offset");
        };
        assert_eq!(negative_offset.seconds(), -(4 * 3600 + 2 * 60));
    }

    #[test]
    fn epoch_integer_round_trip() {
        let epoch = epoch_integer_to_offset(0).unwrap();
        assert_eq!(epoch.local.date.year(), 1970);
        let before = epoch_integer_to_offset(-1).unwrap();
        assert_eq!(before.local.date.year(), 1969);
        assert_eq!(before.local.date.month(), 12);
        assert_eq!(before.local.date.day(), 31);
        assert_eq!(before.local.time.hour(), 23);
        assert_eq!(before.local.time.second(), 59);
        let year_one = epoch_integer_to_offset(MIN_SECONDS).unwrap();
        assert_eq!(year_one.local.date.year(), 0);
        let max = epoch_integer_to_offset(MAX_SECONDS).unwrap();
        assert_eq!(max.local.date.year(), 9999);
        assert!(epoch_integer_to_offset(MAX_SECONDS + 1).is_none());
        assert!(epoch_integer_to_offset(MIN_SECONDS - 1).is_none());
    }

    #[test]
    fn epoch_float_fraction() {
        let value = epoch_float_to_offset(1.5).unwrap();
        assert_eq!(value.local.time.second(), 1);
        assert_eq!(value.local.time.fraction().digits(), "5");
        let negative = epoch_float_to_offset(-1.5).unwrap();
        assert_eq!(negative.local.time.second(), 58);
        assert_eq!(negative.local.time.fraction().digits(), "5");
        assert!(epoch_float_to_offset(f64::NAN).is_none());
        assert!(epoch_float_to_offset(f64::INFINITY).is_none());
        assert!(epoch_float_to_offset(1e12).is_none());
    }
}
