//! Sealed `MessagePack` dialect identities.

/// The advertised UTF-8 input dialect: every `str` payload is validated as UTF-8 during the scan.
pub const MESSAGEPACK_UTF8_DIALECT_ID: &str = "messagepack.utf8@1";
/// The registered-but-unadvertised wire identity: preserves an invalid-UTF-8 `str` as native bytes through the scan;
/// the semantic build then refuses the offending span with `UnsupportedRepresentation`.
pub const MESSAGEPACK_WIRE_DIALECT_ID: &str = "messagepack.wire@1";
/// The duplicate-rejecting input dialect (a registered identity that resolves to no behaviour is a maturity claim with
/// no code behind it). Behaves exactly like `messagepack.utf8@1` (every `str` payload validated as UTF-8) and
/// additionally rejects any map with two keys equal under the native key-equivalence law (integers by mathematical
/// value across marker widths, floats only with floats with signed zeros equal and all NaNs equal, `str` by raw bytes
/// distinct from a byte-equal `bin`, arrays in order, maps as unordered multisets of recursively equal pairs,
/// extensions by signed type code plus exact raw payload; integer `1` distinct from float `1.0`).
pub const MESSAGEPACK_KEY_EQUIVALENCE_DIALECT_ID: &str = "messagepack.key-equivalence@1";
/// The deterministic output profile: shortest exact marker and length for every value, map occurrence order preserved.
pub const MESSAGEPACK_DETERMINISTIC_DIALECT_ID: &str = "messagepack.deterministic@1";
/// The lossy float64 output profile: the deterministic grammar with ONE deliberate divergence — a `Decimal` (jqf's
/// exact non-integer number) is encoded as its nearest IEEE-754 binary64 float instead of refused. The precision loss
/// is IN the identity: naming the dialect is the user's acknowledgment that `0.75` travels as the float64
/// `0x3FE8000000000000`, not as the exact decimal. Every other value keeps the deterministic encoding (integers stay
/// exact integers; floats keep the float32-when-exact / float64 law).
pub const MESSAGEPACK_DETERMINISTIC_FLOAT64_DIALECT_ID: &str = "messagepack.deterministic-float64@1";

/// Which input dialect a decode request named.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Dialect {
    /// `messagepack.utf8@1`: `str` payloads are validated as UTF-8.
    Utf8,
    /// `messagepack.wire@1`: `str` payloads stay raw bytes through the scan.
    Wire,
    /// `messagepack.key-equivalence@1`: `utf8@1` plus the duplicate-key rejection under the native key-equivalence law.
    KeyEquivalence,
}

impl Dialect {
    /// The dialect identity text.
    #[must_use]
    pub(crate) const fn id(self) -> &'static str {
        match self {
            Self::Utf8 => MESSAGEPACK_UTF8_DIALECT_ID,
            Self::Wire => MESSAGEPACK_WIRE_DIALECT_ID,
            Self::KeyEquivalence => MESSAGEPACK_KEY_EQUIVALENCE_DIALECT_ID,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_identities_are_the_written_names() {
        assert_eq!(Dialect::Utf8.id(), MESSAGEPACK_UTF8_DIALECT_ID);
        assert_eq!(Dialect::Wire.id(), MESSAGEPACK_WIRE_DIALECT_ID);
        assert_eq!(Dialect::KeyEquivalence.id(), MESSAGEPACK_KEY_EQUIVALENCE_DIALECT_ID);
    }
}
