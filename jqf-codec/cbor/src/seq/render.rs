//! cbor-seq output: an RFC 8742 sequence has NO framing bytes — items are concatenated — so the encoder is cbor's
//! own, selected by the request's payload profile option, with no prefix, no suffix, and no facade seam at all. This
//! stands in contrast to json-seq's `render.rs`, which exists only to carry the RS prefix and LF suffix; here the
//! registry factory delegates straight to cbor's encoder at the selected profile.

use jqf_codec_core::{CodecError, CodecFailureKind, EncodeRequest, ErasedEncoderFactory};
use jqf_resource::ResourceContext;

use super::options::{CborSeqEncodeOptions, CborSeqPayloadProfile, FORMAT_ID, JQF_DIALECT_ID};

/// Registry entry point: reads the payload profile from the request's own option payload, defaulting to
/// `cbor.preferred@1` when options are omitted.
pub(crate) fn create_registered_factory(
    request: EncodeRequest<'_, '_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    let options = match request.options {
        None => CborSeqEncodeOptions::default(),
        Some(payload) => CborSeqEncodeOptions::from_request_payload(payload)?,
    };
    if request.format.as_str() != FORMAT_ID || request.dialect.as_str() != JQF_DIALECT_ID {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    }
    let profile = match options.profile {
        CborSeqPayloadProfile::Source => crate::encode::Profile::Source,
        CborSeqPayloadProfile::Preferred => crate::encode::Profile::Preferred,
        CborSeqPayloadProfile::CoreDeterministic => crate::encode::Profile::CoreDeterministic,
        CborSeqPayloadProfile::LengthFirst => crate::encode::Profile::LengthFirst,
    };
    // The seq framing writes nothing: delegate to cbor's own encoder at the selected profile, so each item is exactly
    // the payload item's bytes.
    crate::encode::create_factory_with_profile(request, profile, resources)
}
