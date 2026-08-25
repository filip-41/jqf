//! Cross-format decimal render and f32→f64 widening.
//!
//! Numbers are not projected ([`crate::project`]): every format has a native number spelling. [`decimal_render_into`]
//! writes [`DecimalText`]'s scientific-string form, plus an optional `.0` suffix so a type-sensitive target re-parses
//! an integer-shaped decimal as a float.
//!
//! [`widen_f32`] is the IEEE widening binary codecs share. f16 widening stays in the CBOR crate.

use alloc::string::String;
use alloc::vec::Vec;

use jqf_data::DecimalText;

use crate::{CodecError, CodecFailureKind};

/// Renders `coefficient * 10^-scale` as its canonical number text into `out`.
///
/// With `reparse_suffix` set, an integer-shaped rendering (no `.`, `e`, or `E`) is re-spelled with a trailing `.0`, so
/// a type-sensitive target re-parses the decimal as a float rather than an integer. Fails with
/// `UnsupportedRepresentation` when the coefficient carries no digits or the exponent is out of the representable range
/// — both mean the caller was handed a value no decoder produces.
pub fn decimal_render_into(
    coefficient: &str,
    scale: i64,
    reparse_suffix: bool,
    out: &mut Vec<u8>,
) -> Result<(), CodecError> {
    let Some(text) = DecimalText::new(coefficient, scale) else {
        return Err(CodecError::new(CodecFailureKind::UnsupportedRepresentation));
    };
    let mut needs_suffix = reparse_suffix;
    for piece in text.pieces() {
        // Equivalent to the whole-text `contains(['.', 'e', 'E'])`: the pieces partition the rendering, and all three
        // bytes are < 0x80 so none can hide inside a UTF-8 continuation byte.
        if needs_suffix && (piece.contains(&b'.') || piece.contains(&b'e') || piece.contains(&b'E')) {
            needs_suffix = false;
        }
        let text =
            core::str::from_utf8(piece).map_err(|_| CodecError::new(CodecFailureKind::UnsupportedRepresentation))?;
        out.extend_from_slice(text.as_bytes());
    }
    if needs_suffix {
        out.extend_from_slice(b".0");
    }
    Ok(())
}

/// Owned wrapper over [`decimal_render_into`]. Returns `None` on the same unrenderable cases.
#[must_use]
pub fn decimal_render(coefficient: &str, scale: i64, reparse_suffix: bool) -> Option<String> {
    // ONE spelling-law implementation: the owned wrapper stages through the streaming renderer instead of repeating its
    // piece loop, so the suffix policy and piece walk cannot drift between the two entry points.
    let mut out = Vec::new();
    decimal_render_into(coefficient, scale, reparse_suffix, &mut out).ok()?;
    String::from_utf8(out).ok()
}

/// Widens one exact IEEE f32 bit pattern to binary64: sign preserved, the f32 exponent re-biased (subnormals
/// renormalized), and the fraction extended by `<< 29`. The manual NaN handling is the point:
/// `f64::from(f32::from_bits(bits))` also widens, but it QUIETLY collapses every signaling NaN and narrows every
/// payload to the canonical quiet NaN, losing the f32 NaN's exact bits — the wire law keeps the payload.
#[must_use]
pub fn widen_f32(bits: u32) -> u64 {
    let sign = u64::from(bits >> 31) << 63;
    let exp = u64::from((bits >> 23) & 0xff);
    let frac = u64::from(bits & 0x7f_ffff);
    match exp {
        0xff => sign | (0x7ff << 52) | (frac << 29),
        0 => {
            if frac == 0 {
                sign
            } else {
                // Subnormal: value = frac * 2^-149. The normalized binary64 exponent field is `p + 874`; the stored
                // fraction is the widened significand below its implicit bit 52.
                let p = frac.ilog2(); // highest set bit index (0..=22)
                let normalized = (frac << (52 - p)) & ((1u64 << 52) - 1);
                sign | ((u64::from(p) + 874) << 52) | normalized
            }
        }
        exp => sign | ((exp + 896) << 52) | (frac << 29),
    }
}

#[cfg(test)]
mod tests {
    use super::{decimal_render, decimal_render_into, widen_f32};
    use alloc::borrow::ToOwned;
    use alloc::string::String;
    use alloc::vec::Vec;

    /// Renders through the STREAMING path (the one the TOML and delimited encoders call), so the spelling-law corpus
    /// pins the hot route rather than the owned-`String` wrapper.
    fn render(coefficient: &str, scale: i64, reparse_suffix: bool) -> Option<String> {
        let mut out = Vec::new();
        decimal_render_into(coefficient, scale, reparse_suffix, &mut out).ok()?;
        core::str::from_utf8(&out).ok().map(str::to_owned)
    }

    #[test]
    fn renders_the_canonical_text() {
        assert_eq!(render("15", 1, false).as_deref(), Some("1.5"));
        assert_eq!(render("150", 2, false).as_deref(), Some("1.50"));
        assert_eq!(render("1", -2, false).as_deref(), Some("1E+2"));
        assert_eq!(render("1", 400, false).as_deref(), Some("1E-400"));
        assert_eq!(render("0", 2, false).as_deref(), Some("0.00"));
    }

    #[test]
    fn the_reparse_suffix_applies_only_to_integer_shaped_rendering() {
        // `5.0` decodes canonically to coefficient 50 at scale 1 → "5.0" (already a float spelling), while `5` at
        // scale 1 is 0.5 → "0.5" (also fine). The `.0` suffix matters for an integral DECIMAL that canonicalizes to
        // an integer-shaped rendering — e.g. coefficient 5 at scale 0 is the decimal 5, which renders "5" and would
        // reparse as an integer; the suffix keeps it a float spelling.
        assert_eq!(render("50", 1, true).as_deref(), Some("5.0"));
        assert_eq!(render("5", 0, true).as_deref(), Some("5.0"));
        assert_eq!(render("5", 0, false).as_deref(), Some("5"));
        assert_eq!(render("5", 1, true).as_deref(), Some("0.5"));
        assert_eq!(render("1", -16, true).as_deref(), Some("1E+16"));
        assert_eq!(render("15", 1, true).as_deref(), Some("1.5"));
    }

    #[test]
    fn an_empty_coefficient_is_unrenderable() {
        assert!(render("", 0, true).is_none());
        assert!(render("-", 0, true).is_none());
    }

    #[test]
    fn the_owned_string_wrapper_agrees_with_the_streaming_path() {
        // The YAML encoder still renders through `decimal_render`; pin that its bytes equal the streaming path's across
        // the spelling corpus.
        for (coefficient, scale, reparse_suffix) in [
            ("15", 1, false),
            ("150", 2, false),
            ("1", -2, true),
            ("1", 400, false),
            ("0", 2, true),
            ("50", 1, true),
            ("5", 0, true),
            ("5", 0, false),
            ("5", 1, true),
            ("1", -16, true),
            ("1", 323, false),
            ("-10250", 3, true),
        ] {
            assert_eq!(
                decimal_render(coefficient, scale, reparse_suffix).as_deref(),
                render(coefficient, scale, reparse_suffix).as_deref(),
                "decimal_render vs decimal_render_into for ({coefficient}, {scale}, {reparse_suffix})"
            );
        }
    }

    #[test]
    fn widen_f32_preserves_sign_exponent_and_nan_payload() {
        assert_eq!(widen_f32(0x3f80_0000), 0x3ff0_0000_0000_0000);
        assert_eq!(widen_f32(0x8000_0000), 0x8000_0000_0000_0000);
        assert_eq!(widen_f32(0x7f80_0000), 0x7ff0_0000_0000_0000);
        assert_eq!(widen_f32(0xff80_0000), 0xfff0_0000_0000_0000);
        assert_eq!(widen_f32(0x7fc0_0000), 0x7ff8_0000_0000_0000);
        assert_eq!(widen_f32(0x0000_0001), 0x36a0_0000_0000_0000);
        assert_eq!(widen_f32(0x0040_0000), 0x3800_0000_0000_0000);
    }
}
