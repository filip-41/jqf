//! JSON carries no tag surface: every registration installs [`NoTagsValidator`].

use jqf_codec_core::{CodecError, EncodeRequest, ErasedTagValidator};
use jqf_resource::ResourceContext;

pub(crate) fn create_validator(
    request: EncodeRequest<'_, '_>,
    _resources: &mut ResourceContext<'_>,
) -> Result<ErasedTagValidator, CodecError> {
    crate::validate_target(request)?;
    ErasedTagValidator::try_new_validator(|| Ok(jqf_codec_core::NoTagsValidator))
}

/// The commented-JSON target guard for the SAME `NoTags` law: the strict-JSON guard above declines every other format,
/// so the jsonc/json5 registrations name their own factories here instead of reusing it — otherwise a tag request
/// targeting their own profiles would be refused as a foreign target. The guards mirror each encoder factory's
/// `expect_target`.
pub(crate) fn create_jsonc_validator(
    request: EncodeRequest<'_, '_>,
    _resources: &mut ResourceContext<'_>,
) -> Result<ErasedTagValidator, CodecError> {
    guarded_no_tags_validator(request, super::jsonc::FORMAT_ID, &super::jsonc::DIALECT_TEXTS)
}

pub(crate) fn create_json5_validator(
    request: EncodeRequest<'_, '_>,
    _resources: &mut ResourceContext<'_>,
) -> Result<ErasedTagValidator, CodecError> {
    guarded_no_tags_validator(request, super::json5::FORMAT_ID, &super::json5::DIALECT_TEXTS)
}

fn guarded_no_tags_validator(
    request: EncodeRequest<'_, '_>,
    format: &str,
    dialects: &[&str],
) -> Result<ErasedTagValidator, CodecError> {
    request.expect_target(format, dialects)?;
    ErasedTagValidator::try_new_validator(|| Ok(jqf_codec_core::NoTagsValidator))
}
