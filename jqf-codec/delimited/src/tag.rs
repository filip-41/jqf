//! Delimited target tag validation: authoritative tag absence.

use jqf_codec_core::{CodecError, CodecFailureKind, EncodeRequest, ErasedTagValidator};
use jqf_resource::ResourceContext;

pub(crate) fn create_validator(
    request: EncodeRequest<'_, '_>,
    _resources: &mut ResourceContext<'_>,
) -> Result<ErasedTagValidator, CodecError> {
    // A tag validator is built from an encode request and validates the TARGET output profile. It must accept either
    // output family (csv or tsv): the two registrations share this factory, and each request names exactly one
    // format/dialect pair.
    let serves_csv = request.format.as_str() == crate::FORMAT_ID
        && [
            crate::JQF_RFC4180_DIALECT_ID,
            crate::JQF_RFC4180_HEADER_DIALECT_ID,
            crate::JQF_UTF8_DIALECT_ID,
            crate::JQF_UTF8_HEADER_DIALECT_ID,
        ]
        .contains(&request.dialect.as_str());
    let tsv_ok = request.format.as_str() == crate::TSV_FORMAT_ID
        && [crate::TSV_JQF_LF_DIALECT_ID, crate::TSV_JQF_LF_HEADER_DIALECT_ID].contains(&request.dialect.as_str());
    if !(serves_csv || tsv_ok) {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    }
    ErasedTagValidator::try_new_validator(|| Ok(jqf_codec_core::NoTagsValidator))
}
