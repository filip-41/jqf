//! cbor-seq (RFC 8742) identity spellings and normalized option payloads.
//!
//! The registered surface is deliberately narrow: ONE input dialect (`cbor-seq.rfc8742-generic@1`, mirroring
//! `cbor.rfc8949-generic@1` — the payload law is cbor's generic dialect, the FRAMING law is RFC 8742) and ONE output
//! dialect (`cbor-seq.jqf@1`). The payload PROFILE is carried as an option on the output request rather than minting
//! four cbor-seq output dialects beside cbor's `preferred`/`core-deterministic`/`length-first`/ `source`: the profile
//! is not a framing fact, so it stays out of the format's identity.

use jqf_codec_core::{CodecError, CodecFailureKind};

/// Stable cbor-seq (RFC 8742) format identity text.
pub const FORMAT_ID: &str = "cbor-seq";
/// Stable RFC 8742 generic input dialect identity text.
pub const RFC8742_GENERIC_DIALECT_ID: &str = "cbor-seq.rfc8742-generic@1";
/// Stable cbor-seq output dialect identity text.
pub const JQF_DIALECT_ID: &str = "cbor-seq.jqf@1";

/// One of cbor's four payload output profiles, selected as a cbor-seq ENCODE OPTION. Recovery is intentionally absent
/// as an input dialect: with no delimiter, the next item's start is knowable only by having decoded the previous one,
/// so a malformed item leaves no resynchronization point (withheld under portfolio §3.12: a registration advertises
/// only executable operations).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CborSeqPayloadProfile {
    /// `cbor.source@1`: the item bytes are echoed verbatim.
    Source,
    /// `cbor.preferred@1`: canonical preferred encoding (the default).
    #[default]
    Preferred,
    /// `cbor.core-deterministic@1`: deterministic map ordering.
    CoreDeterministic,
    /// `cbor.length-first@1`: length-first deterministic ordering.
    LengthFirst,
}

/// Normalized cbor-seq write-side options: the payload profile (the framing itself writes no bytes — an RFC 8742
/// sequence is concatenated items).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CborSeqEncodeOptions {
    /// The payload profile each item is encoded under.
    pub profile: CborSeqPayloadProfile,
}

impl CborSeqEncodeOptions {
    /// Selects the payload profile.
    #[must_use]
    pub const fn new(profile: CborSeqPayloadProfile) -> Self {
        Self { profile }
    }

    /// Validates an option payload against the request's dialect: cbor-seq output names exactly the one output dialect.
    pub(crate) fn from_request_payload(payload: &(dyn core::any::Any + Send + Sync)) -> Result<Self, CodecError> {
        payload
            .downcast_ref::<CborSeqEncodeOptions>()
            .copied()
            .ok_or_else(|| CodecError::new(CodecFailureKind::RequirementMismatch))
    }
}

#[cfg(test)]
mod tests {
    use super::{CborSeqEncodeOptions, CborSeqPayloadProfile, FORMAT_ID, JQF_DIALECT_ID, RFC8742_GENERIC_DIALECT_ID};

    #[test]
    fn the_registered_identities_keep_their_spellings() {
        assert_eq!(FORMAT_ID, "cbor-seq");
        assert_eq!(RFC8742_GENERIC_DIALECT_ID, "cbor-seq.rfc8742-generic@1");
        assert_eq!(JQF_DIALECT_ID, "cbor-seq.jqf@1");
    }

    #[test]
    fn the_default_payload_profile_is_preferred() {
        assert_eq!(
            CborSeqEncodeOptions::default().profile,
            CborSeqPayloadProfile::Preferred
        );
    }
}
