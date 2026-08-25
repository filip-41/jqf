//! TOML has no tag vocabulary; the validator is [`jqf_codec_core::NoTagsValidator`].

use jqf_codec_core::{CodecError, EncodeRequest, ErasedTagValidator};
use jqf_resource::ResourceContext;

pub(crate) fn create_validator(
    request: EncodeRequest<'_, '_>,
    _resources: &mut ResourceContext<'_>,
) -> Result<ErasedTagValidator, CodecError> {
    // A tag validator is built from an encode request and validates the TARGET output profile. It must accept either
    // output profile: the two registrations share this factory, and each request names exactly one.
    request.expect_target(
        crate::FORMAT_ID,
        &[crate::TOML_JQF_1_0_DIALECT_ID, crate::TOML_JQF_1_1_DIALECT_ID],
    )?;
    ErasedTagValidator::try_new_validator(|| Ok(jqf_codec_core::NoTagsValidator))
}
