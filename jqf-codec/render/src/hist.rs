//! `render.hist@1`: one plain-ASCII frequency histogram per item.
//!
//! The input law: an ARRAY of numbers, or — as the pre-aggregated second shape — an array of objects with EXACTLY
//! the members `value` (a number) and `count` (a non-negative integer). Every other root or element refuses with the
//! named shape problem (`UnsupportedRepresentation`, zero bytes published). An empty array publishes an empty frame,
//! never an error.
//!
//! The bucketing law is fixed defaults only (the composition dial set is frozen at four): TEN equal-width bins over the
//! min..max span; a single bin when every value is equal. Bin edges and labels are deterministic shortest-round-trip
//! binary64 spellings; the last bin is closed on both ends so the maximum lands inside it. A value outside any finite
//! span (NaN, ±infinity) refuses. A span whose DIFFERENCE overflows binary64 (finite endpoints like `-1e308` and
//! `1e308`, or a span so narrow its equal-width step underflows to zero) degenerates to one closed bin over the
//! authored endpoints — equal-width edges are uncomputable there, never approximated with non-finite arithmetic.
//!
//! The layout law: `{label} | {count} | {bar}` rows joined with LF (no trailing LF — the facade appends the single
//! final LF). Labels are left-aligned and padded on the right to the widest label, counts right-aligned to the widest
//! count, and bars are pure ASCII `#`, scaled linearly to a 40-cell peak. No colour and no wide characters anywhere in
//! the frame; labels are rendered as-is with no width compensation.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use jqf_codec_core::CodecError;
use jqf_data::{Number, Value, format_binary64};

use super::error::{contract, unsupported_owned};

/// The bar length, in cells, of the peak bin.
const PEAK_BAR_CELLS: usize = 40;
/// The fixed default bin count over the min..max span.
const BIN_COUNT: usize = 10;

/// One accepted element: a raw magnitude or a pre-aggregated magnitude with its count.
struct Sample {
    value: f64,
    count: u64,
}

/// Renders one item as its histogram frame.
///
/// # Errors
///
/// Returns an `UnsupportedRepresentation` reject for a non-array root, a non-number element, a `{value,count}` object
/// that is not exactly that shape, a count that is not a non-negative integer, a non-finite value, a min..max span
/// wider than binary64's range, or a count total past `u64`; an internal-contract error otherwise. Every refusal fires
/// before any byte of the frame is staged.
pub(crate) fn render(value: &Value) -> Result<String, CodecError> {
    let Value::Array(array) = value.untagged() else {
        return Err(unsupported_owned(
            "hist-shape",
            "the histogram dialect renders an array of numbers (or {\"value\", \"count\"} \
             objects); give it an array, e.g. `[.[] | .ms]`",
        ));
    };
    let mut samples: Vec<Sample> = Vec::new();
    for index in 0..array.len() {
        let element = array.get(index).expect("index below len");
        samples.push(match element.untagged() {
            Value::Number(number) => Sample {
                value: finite_value(index, number)?,
                count: 1,
            },
            Value::Object(object) if object.len() == 2 && object.get("count").is_some() => {
                let Value::Number(number) = object.get("value").ok_or_else(|| element_error(index))? else {
                    return Err(element_error(index));
                };
                let Value::Number(count_number) = object.get("count").expect("checked above") else {
                    return Err(count_error(index));
                };
                let count = count_number
                    .to_i64()
                    .filter(|count| *count >= 0)
                    .ok_or_else(|| count_error(index))?;
                Sample {
                    value: finite_value(index, number)?,
                    #[allow(clippy::cast_sign_loss, reason = "the filter above guarantees a non-negative count")]
                    count: count as u64,
                }
            }
            _ => return Err(element_error(index)),
        });
    }
    let Some((mut min, mut max)) = samples.first().map(|sample| (sample.value, sample.value)) else {
        // Empty input publishes an empty frame, never an error.
        return Ok(String::new());
    };
    for sample in &samples {
        min = min.min(sample.value);
        max = max.max(sample.value);
    }
    let (edges, bins) = bucket(min, max, &samples)?;
    layout(&edges, &bins)
}

/// The element-refusal prose: names the index and what was expected.
fn element_error(index: usize) -> CodecError {
    unsupported_owned(
        "hist-element",
        &format!(
            "element [{index}] is not a number or a {{\"value\", \"count\"}} object with \
             exactly those two members"
        ),
    )
}

fn count_error(index: usize) -> CodecError {
    unsupported_owned(
        "hist-count",
        &format!("element [{index}] has a \"count\" that is not a non-negative integer"),
    )
}

/// The count-total refusal: the samples' counts sum past the largest representable bin total.
fn count_total_error() -> CodecError {
    unsupported_owned(
        "hist-count-total",
        "the histogram's pre-aggregated counts sum past the largest representable \
         bin total (u64)",
    )
}

/// One element's numeric magnitude, refusing NaN and infinities by name.
fn finite_value(index: usize, number: &Number) -> Result<f64, CodecError> {
    let value = numeric_value(number).ok_or_else(|| contract("a decoded number carrying no numeric magnitude"))?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(unsupported_owned(
            "hist-value",
            &format!(
                "element [{index}] is not a finite number (NaN and infinities have no \
                 finite bin edge)"
            ),
        ))
    }
}

/// The binary64 magnitude of a decoded number: machine/boxed integers, exact decimals, floats, and (by string parse)
/// big integers.
fn numeric_value(number: &Number) -> Option<f64> {
    #[allow(
        clippy::cast_precision_loss,
        reason = "bin placement is approximate by design; a magnitude past binary64's \
                  mantissa lands in its nearest bin deterministically"
    )]
    fn integer_magnitude(integer: i64) -> f64 {
        integer as f64
    }
    if let Some(integer) = number.to_i64() {
        return Some(integer_magnitude(integer));
    }
    if let Some(decimal) = number.as_decimal() {
        return Some(decimal.to_f64());
    }
    if let Some(float) = number.as_float() {
        return Some(float.get());
    }
    number.as_integer()?.as_str().parse::<f64>().ok()
}

/// Computes the equal-width bin edges over `min..max` and folds every weighted value into its bin. Returns the shared
/// edges (`len == bins + 1`) and per-bin totals. The accumulation is checked: a set of counts whose sum or a single
/// bin's sum exceeds u64 refuses by name instead of wrapping into a wrong histogram.
#[allow(
    clippy::float_cmp,
    reason = "bit-equal endpoints are exactly the degenerate one-bin span; an \
              epsilon would merge distinct spans"
)]
fn bucket(min: f64, max: f64, samples: &[Sample]) -> Result<(Vec<f64>, Vec<u64>), CodecError> {
    // The counts are user-supplied, so their sum is bounded with checked arithmetic: every bin total is a subset of
    // this total, so one checked fold bounds them all — an overflow refuses by name before any byte is staged, never
    // a silent wrap (or a debug-build panic).
    let mut total = 0_u64;
    for sample in samples {
        total = total.checked_add(sample.count).ok_or_else(count_total_error)?;
    }
    if min == max {
        // Degenerate span: one bin holds everything, labeled by the value.
        return Ok((vec![min], vec![total]));
    }
    let span = max - min;
    if !span.is_finite() {
        // Finite endpoints whose difference overflows binary64 (e.g. -1e308..1e308): equal-width edges would be
        // non-finite, so one closed bin over the authored endpoints holds every count and the maximum still lands
        // inside a bin.
        return Ok((vec![min, max], vec![total]));
    }
    let width = span / f64::from(u16::try_from(BIN_COUNT).expect("a small constant"));
    if width == 0.0 {
        // The span is so narrow its equal-width step underflows to zero (near-subnormal endpoints like 5e-324..1e-323):
        // dividing by it would place bins by NaN/saturating-cast accident, so the same degenerate one-bin law as the
        // overflow arm holds every count over the authored endpoints.
        return Ok((vec![min, max], vec![total]));
    }
    let mut bins = vec![0u64; BIN_COUNT];
    for sample in samples {
        // Clamp the maximum into the last (closed) bin; float rounding at interior edges can land a value one bin high,
        // which clamping also absorbs without ever losing a count.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the ratio is non-negative and clamped into the bin range"
        )]
        let index = (((sample.value - min) / width) as usize).min(BIN_COUNT - 1);
        bins[index] = bins[index].checked_add(sample.count).ok_or_else(count_total_error)?;
    }
    let edges = (0..=BIN_COUNT)
        .map(|step| min + width * f64::from(u16::try_from(step).expect("a small constant")))
        .collect();
    Ok((edges, bins))
}

/// Lays out the frame: label column, right-aligned count column, ASCII bar scaled to the widest bin.
fn layout(edges: &[f64], bins: &[u64]) -> Result<String, CodecError> {
    let peak = bins.iter().copied().max().unwrap_or(0);
    let labels: Vec<String> = match edges.len() {
        0 => return Err(contract("a histogram with no bins")),
        1 => vec![spell(edges[0])?],
        _ => (0..bins.len())
            .map(|bin| {
                let low = spell(edges[bin])?;
                let high = spell(edges[bin + 1])?;
                let close = if bin + 1 == bins.len() { ']' } else { ')' };
                Ok(format!("[{low}, {high}{close}"))
            })
            .collect::<Result<Vec<String>, CodecError>>()?,
    };
    let label_width = labels.iter().map(String::len).max().unwrap_or(0);
    let count_width = bins.iter().map(|count| count.to_string().len()).max().unwrap_or(0);
    let mut out = String::new();
    for (bin, (count, label)) in bins.iter().zip(&labels).enumerate() {
        if bin > 0 {
            out.push('\n');
        }
        out.push_str(label);
        pad(&mut out, label_width - label.len());
        out.push_str(" | ");
        let spelled = count.to_string();
        pad(&mut out, count_width - spelled.len());
        out.push_str(&spelled);
        out.push_str(" |");
        let cells = scale(*count, peak);
        if cells > 0 {
            out.push(' ');
            for _ in 0..cells {
                out.push('#');
            }
        }
    }
    Ok(out)
}

/// Appends `width` spaces.
fn pad(out: &mut String, width: usize) {
    for _ in 0..width {
        out.push(' ');
    }
}

/// The bar length of one bin: linearly scaled to the peak; an empty bin renders no cells.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "counts are far under f64's exact-integer range at the 40-cell peak"
)]
fn scale(count: u64, peak: u64) -> usize {
    if peak == 0 || count == 0 {
        return 0;
    }
    let cells = (count as f64 / peak as f64 * PEAK_BAR_CELLS as f64).ceil();
    (cells as usize).clamp(1, PEAK_BAR_CELLS)
}

/// The shortest-round-trip spelling of one finite bin edge.
fn spell(value: f64) -> Result<String, CodecError> {
    let text = format_binary64(value).ok_or_else(|| contract("a bin edge with no spelling"))?;
    Ok(text.as_str().to_owned())
}
