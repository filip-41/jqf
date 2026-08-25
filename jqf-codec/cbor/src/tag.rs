//! CBOR target tag validation: the `cbor:tag:<n>` / `cbor:simple:<n>` identity law for an encode request's output
//! profile.
//!
//! A tag is valid for CBOR output exactly when it is a `cbor:tag:<n>` with an unsigned 64-bit number spelled
//! canonically (no leading zeros), or a `cbor:simple:<n>` in the representable set (`0..=19`, `23`, `32..=255`). The
//! 20..=22 identities are core Bool/Null, not retained wrappers, and 24..=31 are reserved. Anything else — a foreign
//! `TagId`, a malformed number — is an invalid tag for the target.

use jqf_codec_core::{CodecError, CodecFailureKind, EncodeRequest, ErasedTagValidator, TagValidator};
use jqf_data::TagId;
use jqf_resource::ResourceContext;

use crate::encode::Profile;

struct CborTagValidator;

impl TagValidator for CborTagValidator {
    fn validate(&self, tags: &[&TagId]) -> Result<(), CodecError> {
        for tag in tags {
            let text = tag.as_str();
            if let Some(number) = text.strip_prefix("cbor:tag:") {
                // The CANONICAL spelling only: `cbor:tag:05` and `cbor:tag:5` are distinct TagId texts that would both
                // emit wire tag 5 — a colliding tag set. The shared canonical parse below is the SAME law the
                // encoder's own emission runs, so the two halves cannot disagree (a leading `+` is the same class). The
                // decoder spells canonically, so only hand-built documents can hit the rejected forms.
                crate::encode::parse_tag_number(number).map_err(|_| CodecError::new(CodecFailureKind::InvalidTag))?;
                continue;
            }
            if let Some(number) = text.strip_prefix("cbor:simple:") {
                // The same canonical spelling law as the tag form.
                let value = crate::encode::parse_simple_number(number)
                    .map_err(|_| CodecError::new(CodecFailureKind::InvalidTag))?;
                if !is_representable_simple(value) {
                    return Err(CodecError::new(CodecFailureKind::InvalidTag));
                }
                continue;
            }
            return Err(CodecError::new(CodecFailureKind::InvalidTag));
        }
        Ok(())
    }
}

fn is_representable_simple(value: u8) -> bool {
    matches!(value, 0..=19 | 23 | 32..=255)
}

pub(crate) fn create_validator(
    request: EncodeRequest<'_, '_>,
    _resources: &mut ResourceContext<'_>,
) -> Result<ErasedTagValidator, CodecError> {
    if request.format.as_str() != crate::FORMAT_ID || Profile::from_dialect(request.dialect.as_str()).is_none() {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    }
    ErasedTagValidator::try_new_validator(|| Ok(CborTagValidator))
}

#[cfg(test)]
mod tests {
    use super::CborTagValidator;
    use jqf_codec_core::TagValidator;
    use jqf_data::TagId;

    #[test]
    fn the_number_tag_spelling_is_canonical() {
        // The wire tag is the number, so two texts that parse to the same number collide on the wire: leading zeros and
        // a leading '+' are rejected, the canonical spelling is the only accepted form.
        let validator = CborTagValidator;
        for text in ["cbor:tag:5", "cbor:simple:5", "cbor:tag:0"] {
            let tag = TagId::try_new_unaccounted(text).expect("tag");
            validator
                .validate(&[&tag])
                .unwrap_or_else(|error| panic!("canonical {text:?} must validate: {error}"));
        }
        for text in [
            "cbor:tag:05",
            "cbor:tag:+5",
            "cbor:tag:00",
            "cbor:simple:05",
            "cbor:simple:+5",
        ] {
            let tag = TagId::try_new_unaccounted(text).expect("tag");
            assert!(
                validator.validate(&[&tag]).is_err(),
                "{text:?} must be rejected (it collides with a canonical spelling on the wire)"
            );
        }
        // The representable-simple boundary: 0..=19 | 23 | 32..=255 are the only simple values (20..=22 are core
        // Bool/Null, 24..=31 reserved).
        for text in ["cbor:simple:23", "cbor:simple:32", "cbor:simple:255"] {
            let tag = TagId::try_new_unaccounted(text).expect("tag");
            assert!(
                validator.validate(&[&tag]).is_ok(),
                "{text:?} is a representable simple value"
            );
        }
        for text in ["cbor:simple:20", "cbor:simple:22", "cbor:simple:24", "cbor:simple:31"] {
            let tag = TagId::try_new_unaccounted(text).expect("tag");
            assert!(
                validator.validate(&[&tag]).is_err(),
                "{text:?} is not a representable simple value"
            );
        }
    }
}
