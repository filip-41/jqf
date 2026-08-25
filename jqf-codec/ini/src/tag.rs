//! The flat-config tag validator: the family has no tag vocabulary, so the shared `NoTagsValidator` serves every
//! grammar.

use jqf_codec_core::{CodecError, EncodeRequest, ErasedTagValidator};
use jqf_resource::ResourceContext;

/// Creates the shared no-tags validator.
pub(crate) fn create_validator(
    _request: EncodeRequest<'_, '_>,
    _resources: &mut ResourceContext<'_>,
) -> Result<ErasedTagValidator, CodecError> {
    ErasedTagValidator::try_new_validator(|| Ok(jqf_codec_core::NoTagsValidator))
}
