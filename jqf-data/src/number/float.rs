//! IEEE-754 binary64 values.
//!
//! [`Float`] stores the bits and hands them back. It has no retained spelling — a binary64 *is* its bits. Comparison
//! and arithmetic live with the caller. Rendering lives in `text.rs`.

/// Binary64 bits, including NaN payloads and signed zero.
#[derive(Clone, Copy, Debug)]
pub struct Float(u64);

impl Float {
    /// Stores the exact bits of `value`.
    #[must_use]
    pub const fn new(value: f64) -> Self {
        Self(value.to_bits())
    }

    /// Returns the represented binary64 value.
    #[must_use]
    pub const fn get(self) -> f64 {
        f64::from_bits(self.0)
    }

    /// Returns the exact IEEE-754 bits.
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }
}
