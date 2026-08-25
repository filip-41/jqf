//! Dates and times: RFC 3339 parse/write and civil calendar math.
//!
//! Run with `cargo run -p jqf-data --example temporal`.

use jqf_data::{
    UtcOffset, Value, civil_from_days, days_from_civil, parse_rfc3339, temporal_to_epoch, write_epoch_rfc3339,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `parse_rfc3339` is the one parser this crate exposes.
    let parsed = parse_rfc3339("2024-06-01T12:34:56.125-05:30")?;
    assert_eq!(parsed.local.date.year(), 2024);
    assert_eq!(parsed.local.time.fraction().digits(), "125");
    assert!(matches!(
        parsed.offset,
        UtcOffset::KnownSeconds(offset) if offset.seconds() == -(5 * 3600 + 30 * 60)
    ));

    // The value owns its canonical text: write_text round-trips the spelling.
    let mut text = String::new();
    parsed.write_text(&mut text)?;
    assert_eq!(text, "2024-06-01T12:34:56.125-05:30");

    // Canonical fractions trim trailing zeroes: `.250` retains `25`.
    let trimmed = parse_rfc3339("2024-06-01T00:00:00.250Z")?;
    assert_eq!(trimmed.local.time.fraction().digits(), "25");

    // `-00:00` is RFC 3339's unknown-local marker, distinct from `Z`.
    let unknown = parse_rfc3339("2024-06-01T00:00:00-00:00")?;
    assert!(matches!(unknown.offset, UtcOffset::UnknownLocalOffset));

    // Epoch projection applies the offset; the inverse writes canonical UTC.
    let parsed_value = Value::OffsetDateTime(parsed);
    let (seconds, fraction) = temporal_to_epoch(&parsed_value).ok_or("temporal value")?;
    assert_eq!(fraction, "125");
    assert_eq!(write_epoch_rfc3339(seconds, fraction)?, "2024-06-01T18:04:56.125Z");

    // The civil calendar law: days since 1970-01-01 to (year, month, day) and back, exact for leap years.
    let days = days_from_civil(2024, 2, 29);
    assert_eq!(civil_from_days(days), (2024, 2, 29));
    assert_eq!(days_from_civil(1970, 1, 1), 0);

    println!("RFC 3339 round-trip, epoch projection, and civil math all hold");
    Ok(())
}
