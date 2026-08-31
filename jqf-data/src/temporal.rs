//! Dates and times, plus RFC 3339 parse and write.
//!
//! Each kind owns its own canonical text. [`parse_rfc3339`] is the parser; [`write_epoch_rfc3339`] writes an epoch
//! instant as UTC.
//!
//! [`FractionalSecond::parse`] copies digits from the input and may shorten them. It does not charge a request ledger.

use alloc::string::String;
use core::fmt;
use triomphe::Arc;

/// Failure to construct canonical temporal storage or to parse an RFC 3339 text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TemporalError {
    /// Fractional-second text contained a non-decimal character.
    InvalidFraction,
    /// Allocation failed while retaining normalized fractional digits.
    Allocation,
    /// An RFC 3339 parse text does not satisfy the `date-time` production or its field ranges.
    Syntax,
    /// A component that does not fit its fixed-width RFC 3339 field — an epoch outside the year range `0000..=9999`,
    /// or a borrowed view whose public component fields hold an out-of-range hour, minute or second. Never emitted by
    /// the parse path, which maps a year overflow to [`Self::Syntax`].
    OutOfRange,
}

impl fmt::Display for TemporalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidFraction => "fractional seconds must contain only decimal digits",
            Self::Allocation => "fractional-second allocation failed",
            Self::Syntax => "invalid RFC 3339 date-time syntax",
            Self::OutOfRange => "value does not fit its fixed-width RFC 3339 field",
        })
    }
}

impl core::error::Error for TemporalError {}

/// Arbitrary-precision fractional seconds with trailing zeroes removed.
///
/// Semantic zero (no digits) is its own variant, so the common whole-second case — every `LocalTime`/`LocalDateTime`
/// without a fraction — allocates nothing.
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FractionalSecond(FractionalDigits);

#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum FractionalDigits {
    /// No digits: semantic zero. The derived default.
    #[default]
    Empty,
    /// Trimmed decimal digits in their own shared allocation.
    Digits(Arc<String>),
}

impl FractionalSecond {
    /// Creates a canonical fraction from digits appearing after the decimal point.
    pub fn parse(digits: &str) -> Result<Self, TemporalError> {
        if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(TemporalError::InvalidFraction);
        }
        let normalized = digits.trim_end_matches('0');
        if normalized.is_empty() {
            return Ok(Self(FractionalDigits::Empty));
        }
        let owned = crate::identity::try_copy_str(normalized).map_err(|_| TemporalError::Allocation)?;
        Arc::try_new(owned)
            .map(|shared| Self(FractionalDigits::Digits(shared)))
            .map_err(|_| TemporalError::Allocation)
    }

    /// Returns normalized decimal digits, or an empty string for semantic zero.
    #[must_use]
    pub fn digits(&self) -> &str {
        match &self.0 {
            FractionalDigits::Empty => "",
            FractionalDigits::Digits(shared) => shared.as_str(),
        }
    }
}

/// A proleptic-Gregorian local date.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LocalDate {
    year: u16,
    month: u8,
    day: u8,
}

impl LocalDate {
    /// Creates a date after proleptic-Gregorian range validation.
    #[must_use]
    pub const fn new(year: u16, month: u8, day: u8) -> Option<Self> {
        let max_day = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if is_leap_year(year) => 29,
            2 => 28,
            _ => 0,
        };
        if year <= 9999 && day >= 1 && day <= max_day {
            Some(Self { year, month, day })
        } else {
            None
        }
    }

    /// Returns the proleptic-Gregorian year.
    #[must_use]
    pub const fn year(self) -> u16 {
        self.year
    }

    /// Returns the one-based month.
    #[must_use]
    pub const fn month(self) -> u8 {
        self.month
    }

    /// Returns the one-based day of month.
    #[must_use]
    pub const fn day(self) -> u8 {
        self.day
    }

    /// Appends this date's canonical `YYYY-MM-DD` text to `out`.
    pub fn write_text(self, out: &mut String) -> Result<(), TemporalError> {
        write_digits(out, u32::from(self.year), 4)?;
        push(out, "-")?;
        write_digits(out, u32::from(self.month), 2)?;
        push(out, "-")?;
        write_digits(out, u32::from(self.day), 2)
    }
}

/// Appends the canonical `HH:MM:SS[.fraction]` text of raw time components.
///
/// The one implementation of that spelling: [`LocalTime::write_text`] and the borrowed [`crate::LocalTimeView`] both
/// route here, so an owned value and a document view can never disagree about their own canonical text. A semantic-zero
/// fraction is omitted entirely, which is how the canonical form spells it.
pub(crate) fn write_time_text(
    out: &mut String,
    hour: u8,
    minute: u8,
    second: u8,
    fraction: &str,
) -> Result<(), TemporalError> {
    // Digit-width refuses hour 100; the calendar range refuses hour 24 and 99, which still fit two digits.
    if hour > 23 || minute > 59 || second > 60 {
        return Err(TemporalError::OutOfRange);
    }
    write_digits(out, u32::from(hour), 2)?;
    push(out, ":")?;
    write_digits(out, u32::from(minute), 2)?;
    push(out, ":")?;
    write_digits(out, u32::from(second), 2)?;
    if fraction.is_empty() {
        return Ok(());
    }
    push(out, ".")?;
    push(out, fraction)
}

/// Appends `text` to `out`, reporting a failed reservation rather than aborting.
pub(crate) fn push(out: &mut String, text: &str) -> Result<(), TemporalError> {
    out.try_reserve(text.len()).map_err(|_| TemporalError::Allocation)?;
    out.push_str(text);
    Ok(())
}

/// Appends `value` as exactly `width` zero-padded decimal digits.
///
/// A value wider than its field is [`TemporalError::OutOfRange`], never a silent truncation to the low digits: the
/// borrowed views carry their components as public fields, so an out-of-range hour or minute can reach here without
/// passing a constructor's range check.
fn write_digits(out: &mut String, value: u32, width: u32) -> Result<(), TemporalError> {
    let mut divisor = width
        .checked_sub(1)
        .and_then(|exponent| 10_u32.checked_pow(exponent))
        .ok_or(TemporalError::OutOfRange)?;
    if value / divisor >= 10 {
        return Err(TemporalError::OutOfRange);
    }
    out.try_reserve(width as usize).map_err(|_| TemporalError::Allocation)?;
    while divisor > 0 {
        // `value / divisor % 10` is mathematically one decimal digit, so `from_digit` cannot miss.
        let digit = char::from_digit(value / divisor % 10, 10).expect("a decimal digit");
        out.push(digit);
        divisor /= 10;
    }
    Ok(())
}

const fn is_leap_year(year: u16) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

/// A local wall-clock time with arbitrary fractional-second precision.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LocalTime {
    hour: u8,
    minute: u8,
    second: u8,
    fraction: FractionalSecond,
}

impl LocalTime {
    /// Creates a time after range validation.
    ///
    /// Second 60 keeps its leap label at ANY minute. RFC 3339 §5.7 spells a UTC `23:59:60` leap second through nonzero
    /// offsets, so other local wall times can legally carry the label and a local time has no offset context to rule an
    /// hour/minute out. The label folds onto the next whole second under the POSIX projection — see
    /// [`temporal_to_epoch`] — while field equality keeps it distinct.
    #[must_use]
    pub fn new(hour: u8, minute: u8, second: u8, fraction: FractionalSecond) -> Option<Self> {
        if hour <= 23 && minute <= 59 && second <= 60 {
            Some(Self {
                hour,
                minute,
                second,
                fraction,
            })
        } else {
            None
        }
    }

    /// Returns the hour in `0..=23`.
    #[must_use]
    pub const fn hour(&self) -> u8 {
        self.hour
    }

    /// Returns the minute in `0..=59`.
    #[must_use]
    pub const fn minute(&self) -> u8 {
        self.minute
    }

    /// Returns the second in `0..=60`.
    #[must_use]
    pub const fn second(&self) -> u8 {
        self.second
    }

    /// Returns canonical fractional-second digits.
    #[must_use]
    pub const fn fraction(&self) -> &FractionalSecond {
        &self.fraction
    }

    /// Appends this time's canonical `HH:MM:SS[.fraction]` text to `out`. The fractional part is omitted entirely for a
    /// semantic-zero fraction, which is how the canonical form spells it.
    pub fn write_text(&self, out: &mut String) -> Result<(), TemporalError> {
        write_time_text(out, self.hour, self.minute, self.second, self.fraction.digits())
    }
}

/// A local date and time.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LocalDateTime {
    /// Calendar date.
    pub date: LocalDate,
    /// Wall-clock time.
    pub time: LocalTime,
}

impl LocalDateTime {
    /// Appends this date-time's canonical `YYYY-MM-DDTHH:MM:SS[.fraction]` text to `out`.
    pub fn write_text(&self, out: &mut String) -> Result<(), TemporalError> {
        self.date.write_text(out)?;
        push(out, "T")?;
        self.time.write_text(out)
    }
}

/// A validated known UTC offset strictly inside 24 hours.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct KnownUtcOffset(i32);

impl KnownUtcOffset {
    /// Creates a known offset when its magnitude is strictly below 24 hours.
    #[must_use]
    pub const fn new(seconds: i32) -> Option<Self> {
        if seconds >= -86_399 && seconds <= 86_399 {
            Some(Self(seconds))
        } else {
            None
        }
    }

    /// Returns signed offset seconds.
    #[must_use]
    pub const fn seconds(self) -> i32 {
        self.0
    }
}

/// A known offset or the semantic `-00:00` unknown-local marker.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum UtcOffset {
    /// A validated known offset.
    KnownSeconds(KnownUtcOffset),
    /// An unknown local offset.
    UnknownLocalOffset,
}

impl UtcOffset {
    /// Appends this offset's canonical text to `out`: `Z` for zero, `-00:00` for the unknown-local marker, else
    /// `±HH:MM`.
    ///
    /// RFC 3339 has no sub-minute offset spelling and neither truncation of one is sound: a positive sub-minute offset
    /// would render as `+00:00`, which re-parses as a DIFFERENT instant, and a negative one as `-00:00`, which
    /// re-parses as the unknown-local MARKER. Both signs report [`TemporalError::OutOfRange`] instead of silently
    /// shifting the instant. Encoders that own a stricter grammar (TOML, CBOR tag 0) refuse before delegating and are
    /// unaffected.
    ///
    /// # Errors
    ///
    /// Any known offset whose seconds are not a whole number of minutes.
    pub fn write_text(self, out: &mut String) -> Result<(), TemporalError> {
        let seconds = match self {
            Self::UnknownLocalOffset => return push(out, "-00:00"),
            Self::KnownSeconds(known) if known.seconds() == 0 => return push(out, "Z"),
            Self::KnownSeconds(known) => known.seconds(),
        };
        if seconds.rem_euclid(60) != 0 {
            return Err(TemporalError::OutOfRange);
        }
        push(out, if seconds < 0 { "-" } else { "+" })?;
        let magnitude = seconds.unsigned_abs();
        write_digits(out, magnitude / 3600, 2)?;
        push(out, ":")?;
        write_digits(out, magnitude % 3600 / 60, 2)
    }
}

/// A local date and time paired with its UTC offset.
#[derive(Clone, Debug)]
pub struct OffsetDateTime {
    /// Local portion.
    pub local: LocalDateTime,
    /// Offset semantics.
    pub offset: UtcOffset,
}

impl OffsetDateTime {
    /// Appends this value's canonical RFC 3339 text to `out`.
    pub fn write_text(&self, out: &mut String) -> Result<(), TemporalError> {
        self.local.write_text(out)?;
        self.offset.write_text(out)
    }
}

// ---------------------------------------------------------------------------
// RFC 3339 parse and epoch-to-RFC-3339 write — the one implementation across
// the tree (CBOR tag 0, TOML datetimes, engine `fromrfc3339`/`torfc3339`).

/// Parse one RFC 3339 `date-time` (uppercase `T` / `Z`).
///
/// Anything off the production, including a year outside `0000..=9999`, is [`TemporalError::Syntax`]. `-00:00` is
/// [`UtcOffset::UnknownLocalOffset`].
#[allow(
    clippy::cast_possible_truncation,
    reason = "the two-digit fields are capped at 99 by parse_digits_temporal; max four-digit year is 9999"
)]
pub fn parse_rfc3339(text: &str) -> Result<OffsetDateTime, TemporalError> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 {
        return Err(TemporalError::Syntax);
    }
    let year = parse_digits_temporal(bytes, 0, 4).ok_or(TemporalError::Syntax)?;
    expect_temporal(bytes, 4, b'-').ok_or(TemporalError::Syntax)?;
    let month = parse_digits_temporal(bytes, 5, 2).ok_or(TemporalError::Syntax)? as u8;
    expect_temporal(bytes, 7, b'-').ok_or(TemporalError::Syntax)?;
    let day = parse_digits_temporal(bytes, 8, 2).ok_or(TemporalError::Syntax)? as u8;
    expect_temporal(bytes, 10, b'T').ok_or(TemporalError::Syntax)?;
    let hour = parse_digits_temporal(bytes, 11, 2).ok_or(TemporalError::Syntax)? as u8;
    expect_temporal(bytes, 13, b':').ok_or(TemporalError::Syntax)?;
    let minute = parse_digits_temporal(bytes, 14, 2).ok_or(TemporalError::Syntax)? as u8;
    expect_temporal(bytes, 16, b':').ok_or(TemporalError::Syntax)?;
    let second = parse_digits_temporal(bytes, 17, 2).ok_or(TemporalError::Syntax)? as u8;
    let mut cursor = 19;
    // The digits are borrowed, never copied: an attacker-chosen fraction run is unbounded, and the one retained copy
    // belongs to `FractionalSecond` after normalization has shortened it.
    let mut fraction = "";
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        let start = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        if cursor == start {
            return Err(TemporalError::Syntax);
        }
        fraction = text.get(start..cursor).ok_or(TemporalError::Syntax)?;
    }
    let offset = parse_offset_temporal(bytes, cursor).ok_or(TemporalError::Syntax)?;
    // The constructors re-derive the same leap rule (`LocalDate::new`) and the same hour/minute/second bounds, and
    // second 60 retains its leap label.
    let date = LocalDate::new(year as u16, month, day).ok_or(TemporalError::Syntax)?;
    let time = LocalTime::new(
        hour,
        minute,
        second,
        FractionalSecond::parse(fraction).map_err(|_| TemporalError::Syntax)?,
    )
    .ok_or(TemporalError::Syntax)?;
    Ok(OffsetDateTime {
        local: LocalDateTime { date, time },
        offset,
    })
}

/// Write RFC 3339 UTC text for an epoch instant (seconds since 1970-01-01T00:00:00Z).
///
/// `fraction_digits` must be decimal digits or empty. Trailing zeroes are dropped. Empty after that means no fraction.
/// Always ends in `Z`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "seconds.rem_euclid(86_400) is 0..86399, fits u32; hour/minute/second unsigned"
)]
pub fn write_epoch_rfc3339(seconds: i64, fraction_digits: &str) -> Result<String, TemporalError> {
    let fraction = FractionalSecond::parse(fraction_digits)?;
    let date = civil_from_epoch(seconds.div_euclid(86_400)).ok_or(TemporalError::OutOfRange)?;
    let day_seconds = seconds.rem_euclid(86_400);
    let mut out = String::new();
    date.write_text(&mut out)?;
    push(&mut out, "T")?;
    write_time_text(
        &mut out,
        u8::try_from((day_seconds / 3600) as u32).unwrap_or(0),
        u8::try_from(((day_seconds % 3600) / 60) as u32).unwrap_or(0),
        u8::try_from((day_seconds % 60) as u32).unwrap_or(0),
        fraction.digits(),
    )?;
    push(&mut out, "Z")?;
    Ok(out)
}

/// Date from days since `1970-01-01`, or `None` outside `0000..=9999`.
///
/// RFC 3339 writers use this clamped form. Use [`civil_from_days`] when you need years outside that range.
///
/// # Panics
///
/// Never in practice: the algorithm bounds day to `1..=31`, month to `1..=12`, and the year-range check above rejects
/// every other year before the [`LocalDate::new`] call.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the civil_from_days algorithm bounds day to 1..=31, month to 1..=12, year to 0..=9999 checked"
)]
pub fn civil_from_epoch(days: i64) -> Option<LocalDate> {
    let (year, month, day) = civil_from_days(days);
    if !(0..=9999).contains(&year) {
        return None;
    }
    Some(LocalDate::new(year as u16, month as u8, day as u8).expect("civil_from_epoch year/month/day validated"))
}

/// `(year, month, day)` from days since `1970-01-01`. Year is a signed `i64`, not clamped. Reverse of
/// [`days_from_civil`]. Use [`civil_from_epoch`] when you need `0000..=9999`.
#[must_use]
pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096).div_euclid(365);
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2).div_euclid(153);
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

/// UTC seconds for civil fields plus an offset.
///
/// Unknown-local (`-00:00`) counts as offset zero. The enforced domain is years `0000..=9999` with in-range field
/// values — what every parser and constructor produces — and inside it the result always fits `i64`. Outside it the
/// arithmetic is not defined: debug builds assert the documented civil window (year, month, and day), release builds
/// wrap rather than check this hot conversion path. Non-hot callers use [`try_epoch_seconds_from_civil_parts`].
#[must_use]
pub fn epoch_seconds_from_civil_parts(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    offset_seconds: i64,
) -> i64 {
    debug_assert!(
        (0..=9999).contains(&year) && (1..=12).contains(&month) && (1..=31).contains(&day),
        "civil fields outside the documented RFC 3339 window"
    );
    days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second - offset_seconds
}

/// The checked twin of [`epoch_seconds_from_civil_parts`]: `None` when any civil field lies outside its RFC 3339 domain
/// (year `0000..=9999`, month `1..=12`, day a real calendar date — `1..=31`, month-length and leap-year checked, so
/// February 30th is refused) or when the assembly would overflow, instead of a wrapped number. The leap-second label
/// folds exactly as the unchecked form does. Non-hot callers that would otherwise trust the documented precondition use
/// this one; hot conversion paths keep the unchecked form.
#[must_use]
pub fn try_epoch_seconds_from_civil_parts(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    offset_seconds: i64,
) -> Option<i64> {
    if !(0..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    // A day past its month's length rolls forward under `days_from_civil`; the round-trip refuses it where
    // `LocalDate::new` refuses it too.
    if civil_from_days(days) != (year, month, day) {
        return None;
    }
    let days = days.checked_mul(86_400)?;
    let time = hour
        .checked_mul(3_600)?
        .checked_add(minute.checked_mul(60)?)?
        .checked_add(second)?;
    days.checked_add(time)?.checked_sub(offset_seconds)
}

/// Epoch seconds and fraction digits for a temporal value.
///
/// `LocalDate` is midnight. `LocalTime` and `LocalDateTime` sit on `1970-01-01`. `OffsetDateTime` applies its offset;
/// unknown-local is UTC. `None` only if `value` is not temporal.
///
/// A leap-second label (`second == 60`) folds onto the following whole second, so two field-distinct values can project
/// to one instant — the same many-to-one relation a nonzero offset already creates between its spelling and the `Z`
/// spelling of one instant. The projection answers the REPRESENTED INSTANT; derived `Eq`/`Hash` stay field-based and
/// keep the label distinct by design.
#[must_use]
#[allow(
    clippy::cast_sign_loss,
    reason = "the temporal field values are unsigned from validated parsed digits or constructed types"
)]
pub fn temporal_to_epoch(value: &crate::Value) -> Option<(i64, &str)> {
    let (year, month, day, hour, minute, second, fraction_str, offset_seconds) = match value {
        crate::Value::LocalDate(date) => (
            i64::from(date.year()),
            i64::from(date.month()),
            i64::from(date.day()),
            0,
            0,
            0,
            "",
            0,
        ),
        crate::Value::LocalTime(time) => (
            1970,
            1,
            1,
            i64::from(time.hour()),
            i64::from(time.minute()),
            i64::from(time.second()),
            time.fraction().digits(),
            0,
        ),
        crate::Value::LocalDateTime(dt) => (
            i64::from(dt.date.year()),
            i64::from(dt.date.month()),
            i64::from(dt.date.day()),
            i64::from(dt.time.hour()),
            i64::from(dt.time.minute()),
            i64::from(dt.time.second()),
            dt.time.fraction().digits(),
            0,
        ),
        crate::Value::OffsetDateTime(odt) => {
            let offset = match odt.offset {
                UtcOffset::KnownSeconds(known) => i64::from(known.seconds()),
                UtcOffset::UnknownLocalOffset => 0,
            };
            (
                i64::from(odt.local.date.year()),
                i64::from(odt.local.date.month()),
                i64::from(odt.local.date.day()),
                i64::from(odt.local.time.hour()),
                i64::from(odt.local.time.minute()),
                i64::from(odt.local.time.second()),
                odt.local.time.fraction().digits(),
                offset,
            )
        }
        _ => return None,
    };
    Some((
        epoch_seconds_from_civil_parts(year, month, day, hour, minute, second, offset_seconds),
        fraction_str,
    ))
}

/// Days since `1970-01-01` for a signed-year civil date.
///
/// Reverse of [`civil_from_days`].
#[must_use]
#[allow(
    clippy::cast_sign_loss,
    reason = "the algorithm operates on signed types and produces controlled unsigned results"
)]
pub const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * month_prime + 2).div_euclid(5) + day - 1;
    let doe = yoe * 365 + yoe.div_euclid(4) - yoe.div_euclid(100) + doy;
    era * 146_097 + doe - 719_468
}

fn parse_digits_temporal(bytes: &[u8], start: usize, count: usize) -> Option<u32> {
    let slice = bytes.get(start..start + count)?;
    if !slice.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut value: u32 = 0;
    for byte in slice {
        value = value * 10 + u32::from(*byte - b'0');
    }
    Some(value)
}

fn expect_temporal(bytes: &[u8], index: usize, byte: u8) -> Option<()> {
    (bytes.get(index) == Some(&byte)).then_some(())
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "the two-digit fields are capped at 99 by parse_digits_temporal"
)]
fn parse_offset_temporal(bytes: &[u8], start: usize) -> Option<UtcOffset> {
    match bytes.get(start) {
        Some(b'Z') if bytes.len() == start + 1 => Some(UtcOffset::KnownSeconds(KnownUtcOffset::new(0)?)),
        Some(sign @ (b'+' | b'-')) => {
            let hour = parse_digits_temporal(bytes, start + 1, 2)? as u8;
            expect_temporal(bytes, start + 3, b':')?;
            let minute = parse_digits_temporal(bytes, start + 4, 2)? as u8;
            if bytes.len() != start + 6 || hour > 23 || minute > 59 {
                return None;
            }
            let seconds = i32::from(hour) * 3600 + i32::from(minute) * 60;
            if *sign == b'-' {
                if hour == 0 && minute == 0 {
                    return Some(UtcOffset::UnknownLocalOffset);
                }
                return Some(UtcOffset::KnownSeconds(KnownUtcOffset::new(-seconds)?));
            }
            Some(UtcOffset::KnownSeconds(KnownUtcOffset::new(seconds)?))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FractionalSecond, KnownUtcOffset, LocalDate, LocalDateTime, LocalTime, OffsetDateTime, TemporalError, UtcOffset,
    };
    use alloc::string::String;

    fn time(hour: u8, minute: u8, second: u8, fraction: &str) -> LocalTime {
        LocalTime::new(
            hour,
            minute,
            second,
            FractionalSecond::parse(fraction).expect("fraction"),
        )
        .expect("time")
    }

    fn rendered(offset: UtcOffset, fraction: &str) -> String {
        let mut out = String::new();
        OffsetDateTime {
            local: LocalDateTime {
                date: LocalDate::new(2026, 6, 15).expect("date"),
                time: time(12, 34, 56, fraction),
            },
            offset,
        }
        .write_text(&mut out)
        .expect("text");
        out
    }

    fn known(seconds: i32) -> UtcOffset {
        UtcOffset::KnownSeconds(KnownUtcOffset::new(seconds).expect("offset"))
    }

    /// A component wider than its fixed-width field must be refused, not truncated to its low digits: the borrowed
    /// views carry hour, minute and second as public fields, so a value the constructors would have refused can still
    /// reach the component writer, and hour `99` rendered as `99` is wrong while hour `100` rendered as `00` is
    /// silently plausible.
    #[test]
    fn a_component_too_wide_for_its_field_is_refused_not_truncated() {
        let mut out = String::new();
        assert_eq!(super::write_digits(&mut out, 100, 2), Err(TemporalError::OutOfRange));
        assert_eq!(super::write_digits(&mut out, 10_000, 4), Err(TemporalError::OutOfRange));
        assert!(out.is_empty(), "a refused field writes nothing");

        super::write_digits(&mut out, 99, 2).expect("the widest two-digit value fits");
        assert_eq!(out, "99");
    }

    #[test]
    fn write_time_text_refuses_calendar_illegal_hours() {
        use crate::LocalTimeView;

        let mut out = String::new();
        assert_eq!(
            LocalTimeView {
                hour: 24,
                minute: 0,
                second: 0,
                fraction: "",
            }
            .write_text(&mut out),
            Err(TemporalError::OutOfRange)
        );
        assert_eq!(
            LocalTimeView {
                hour: 99,
                minute: 0,
                second: 0,
                fraction: "",
            }
            .write_text(&mut out),
            Err(TemporalError::OutOfRange)
        );
        assert!(out.is_empty(), "a refused hour writes nothing");
        LocalTimeView {
            hour: 23,
            minute: 0,
            second: 0,
            fraction: "",
        }
        .write_text(&mut out)
        .expect("hour 23 is in range");
        assert_eq!(out, "23:00:00");
    }

    /// A sub-minute offset has no lossless RFC 3339 spelling in EITHER direction: positive would truncate to `+00:00`
    /// (a different instant) and negative to `-00:00` (the unknown-local marker). The writer reports the loss instead
    /// of emitting text that changes meaning.
    #[test]
    fn a_sub_minute_offset_is_refused_in_both_directions() {
        let mut out = String::new();
        assert_eq!(known(-30).write_text(&mut out), Err(TemporalError::OutOfRange));
        assert_eq!(known(30).write_text(&mut out), Err(TemporalError::OutOfRange));
        assert!(out.is_empty(), "a refused offset writes nothing");
        assert_eq!(rendered(known(-60), ""), "2026-06-15T12:34:56-00:01");
    }

    #[test]
    fn canonical_text_zero_pads_every_field() {
        let mut out = String::new();
        LocalDate::new(7, 1, 2)
            .expect("date")
            .write_text(&mut out)
            .expect("text");
        assert_eq!(out, "0007-01-02");
        let mut out = String::new();
        time(1, 2, 3, "").write_text(&mut out).expect("text");
        assert_eq!(out, "01:02:03");
    }

    #[test]
    fn canonical_text_spells_every_offset_form() {
        assert_eq!(rendered(known(0), ""), "2026-06-15T12:34:56Z");
        assert_eq!(rendered(known(2 * 3600), ""), "2026-06-15T12:34:56+02:00");
        assert_eq!(rendered(known(-(5 * 3600 + 30 * 60)), ""), "2026-06-15T12:34:56-05:30");
        assert_eq!(rendered(UtcOffset::UnknownLocalOffset, ""), "2026-06-15T12:34:56-00:00");
    }

    /// A semantic-zero fraction leaves no decimal point behind, and a retained one keeps its normalized digits.
    #[test]
    fn canonical_text_omits_an_empty_fraction() {
        assert_eq!(rendered(known(0), "500"), "2026-06-15T12:34:56.5Z");
        assert_eq!(rendered(known(0), "0"), "2026-06-15T12:34:56Z");
        assert_eq!(rendered(known(0), "000123"), "2026-06-15T12:34:56.000123Z");
    }

    #[test]
    fn parse_rfc3339_valid_forms() {
        let v = super::parse_rfc3339("1970-01-01T00:00:00Z").expect("epoch");
        assert_eq!(v.local.date.year(), 1970);
        assert_eq!(v.local.date.month(), 1);
        assert_eq!(v.local.date.day(), 1);

        let v = super::parse_rfc3339("2024-06-01T12:34:56.123Z").expect("fraction");
        assert_eq!(v.local.time.fraction().digits(), "123");

        let v = super::parse_rfc3339("2024-06-01T12:34:56-05:30").expect("neg offset");
        assert!(matches!(v.offset, UtcOffset::KnownSeconds(s) if s.seconds() < 0));

        let v = super::parse_rfc3339("2024-06-01T12:34:56+02:00").expect("pos offset");
        assert!(matches!(v.offset, UtcOffset::KnownSeconds(s) if s.seconds() == 7200));

        let v = super::parse_rfc3339("2024-06-01T12:34:56-00:00").expect("unknown local");
        assert!(matches!(v.offset, UtcOffset::UnknownLocalOffset));
    }

    #[test]
    fn parse_rfc3339_rejects() {
        assert!(matches!(
            super::parse_rfc3339("2024-06-01t12:34:56z"),
            Err(TemporalError::Syntax)
        ));
        assert!(matches!(
            super::parse_rfc3339("2024-06-01 12:34:56Z"),
            Err(TemporalError::Syntax)
        ));
        assert!(matches!(
            super::parse_rfc3339("2024-13-01T12:34:56Z"),
            Err(TemporalError::Syntax)
        ));
        assert!(matches!(super::parse_rfc3339("not a date"), Err(TemporalError::Syntax)));
        assert!(matches!(super::parse_rfc3339(""), Err(TemporalError::Syntax)));
    }

    #[test]
    fn parse_rfc3339_out_of_range_year() {
        assert!(matches!(
            super::parse_rfc3339("10000-01-01T00:00:00Z"),
            Err(TemporalError::Syntax)
        ));
    }

    #[test]
    fn write_epoch_rfc3339_known_values() {
        assert_eq!(
            super::write_epoch_rfc3339(0, "").expect("epoch"),
            "1970-01-01T00:00:00Z"
        );
        assert_eq!(
            super::write_epoch_rfc3339(1_425_599_507, "").expect("known"),
            "2015-03-05T23:51:47Z"
        );
    }

    #[test]
    fn write_epoch_rfc3339_with_fraction() {
        assert_eq!(
            super::write_epoch_rfc3339(0, "5").expect("fraction"),
            "1970-01-01T00:00:00.5Z"
        );
        assert_eq!(
            super::write_epoch_rfc3339(0, "0").expect("zero fraction"),
            "1970-01-01T00:00:00Z"
        );
        assert_eq!(
            super::write_epoch_rfc3339(1, "500").expect("normalized"),
            "1970-01-01T00:00:01.5Z"
        );
        assert!(super::write_epoch_rfc3339(0, "abc").is_err());
    }

    #[test]
    fn temporal_to_epoch_conversions() {
        use crate::Value;
        let dt = LocalDateTime {
            date: LocalDate::new(2024, 6, 1).expect("date"),
            time: LocalTime::new(12, 34, 56, FractionalSecond::parse("123").expect("f")).expect("time"),
        };
        let value = Value::LocalDateTime(dt);
        let (sec, frac) = super::temporal_to_epoch(&value).expect("convert");
        assert_eq!(sec, 1_717_245_296, "2024-06-01T12:34:56Z");
        assert_eq!(frac, "123");
    }

    #[test]
    fn write_epoch_rfc3339_round_trips_with_parse() {
        let epoch = 1_425_599_507i64;
        let text = super::write_epoch_rfc3339(epoch, "").expect("write");
        let parsed = super::parse_rfc3339(&text).expect("parse");
        assert_eq!(
            super::write_epoch_rfc3339(epoch, parsed.local.time.fraction().digits()).expect("rewrite"),
            text
        );
    }

    /// The leap-second law, both halves: the POSIX projection folds the label onto the represented instant (so
    /// `15:59:60-08:00` and `1991-01-01T00:00:00Z` project identically), while field equality keeps a labelled time
    /// distinct from its rolled-forward spelling. RFC 3339 §5.7's nonzero-offset example must keep parsing — the
    /// label is legal away from `23:59` local, so acceptance cannot be narrowed to one hour/minute.
    #[test]
    fn leap_second_folds_to_the_instant_but_stays_field_distinct() {
        use crate::Value;

        let labelled = LocalTime::new(12, 34, 60, FractionalSecond::default()).expect("labelled minute");
        let rolled = LocalTime::new(12, 35, 0, FractionalSecond::default()).expect("rolled");
        assert_ne!(labelled, rolled, "the label is a distinct field value");
        let (labelled_secs, _) = super::temporal_to_epoch(&Value::LocalTime(labelled)).expect("labelled folds");
        let (rolled_secs, _) = super::temporal_to_epoch(&Value::LocalTime(rolled)).expect("rolled folds");
        assert_eq!(labelled_secs, rolled_secs);

        let parsed = super::parse_rfc3339("1990-12-31T15:59:60-08:00").expect("leap example");
        assert_eq!(parsed.local.time.second(), 60);
        let (secs, _) = super::temporal_to_epoch(&Value::OffsetDateTime(parsed)).expect("leap folds");
        assert_eq!(secs, 662_688_000, "1991-01-01T00:00:00Z");
        let midnight = super::parse_rfc3339("1991-01-01T00:00:00Z").expect("midnight");
        let (midnight_secs, _) = super::temporal_to_epoch(&Value::OffsetDateTime(midnight)).expect("midnight folds");
        assert_eq!(secs, midnight_secs);
    }

    /// The checked twin must enforce what `LocalDate::new` enforces: a day past its month's length is refused even
    /// though `1..=31` holds.
    #[test]
    fn try_epoch_seconds_refuses_impossible_calendar_days() {
        assert_eq!(super::try_epoch_seconds_from_civil_parts(2024, 4, 31, 0, 0, 0, 0), None);
        assert_eq!(super::try_epoch_seconds_from_civil_parts(2024, 2, 30, 0, 0, 0, 0), None);
        assert_eq!(super::try_epoch_seconds_from_civil_parts(2023, 2, 29, 0, 0, 0, 0), None);
        assert!(super::try_epoch_seconds_from_civil_parts(2024, 2, 29, 0, 0, 0, 0).is_some());
    }

    #[test]
    fn try_epoch_seconds_still_folds_the_leap_second_label() {
        let folded = super::try_epoch_seconds_from_civil_parts(2016, 12, 31, 23, 59, 60, 0).expect("labelled");
        let next = super::try_epoch_seconds_from_civil_parts(2017, 1, 1, 0, 0, 0, 0).expect("next second");
        assert_eq!(folded, next);
    }
}
