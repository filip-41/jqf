//! The `MessagePack` tag validator.
//!
//! The extension identity is `"msgpack:ext:<n>"` with `<n>` a CANONICAL signed decimal over the signed 8-bit space —
//! no leading zeros, no `+`, no invented `@1` suffix. The validator refuses every non-canonical spelling
//! (`msgpack:ext:+1`, `msgpack:ext:-01`) because two distinct `TagId` texts that emit the same wire type code are a
//! colliding tag set (the encode-side tag law). The mapping is injective over the accepted native space: decode spells
//! canonically, encode accepts only canonical.

use jqf_codec_core::{CodecError, CodecFailureKind, EncodeRequest, ErasedTagValidator, TagValidator};
use jqf_resource::ResourceContext;

/// Creates the `MessagePack` tag validator.
pub(crate) fn create_validator(
    _request: EncodeRequest<'_, '_>,
    _resources: &mut ResourceContext<'_>,
) -> Result<ErasedTagValidator, CodecError> {
    ErasedTagValidator::try_new_validator(|| Ok(MessagepackTagValidator))
}

/// The canonical `msgpack:ext:<n>` spelling validator.
struct MessagepackTagValidator;

impl TagValidator for MessagepackTagValidator {
    fn validate(&self, tags: &[&jqf_data::TagId]) -> Result<(), CodecError> {
        for tag in tags {
            let Some(rest) = tag.as_str().strip_prefix("msgpack:ext:") else {
                continue; // a foreign tag is the encoder's refusal, not a validity error
            };
            if parse_canonical_type(rest).is_none() {
                return Err(CodecError::new(CodecFailureKind::InvalidTag));
            }
        }
        Ok(())
    }
}

/// Parses one canonical signed decimal over the signed 8-bit space: `-128..=-1` or `0..=127`, no leading zeros, no `+`.
/// Returns the wire type code, or `None` for a non-canonical spelling.
pub(crate) fn parse_canonical_type(text: &str) -> Option<i8> {
    if text.is_empty() {
        return None;
    }
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    if digits.is_empty() {
        return None;
    }
    // No leading zeros: a single `0` is the only zero-spelling allowed.
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    // Zero spells only `0`: `-0` negates to the same wire type code and would break the mapping's injectivity over the
    // accepted space.
    if negative && digits == "0" {
        return None;
    }
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let value: i64 = digits.parse().ok()?;
    let value = if negative { -value } else { value };
    if !(-128..=127).contains(&value) {
        return None;
    }
    i8::try_from(value).ok()
}

#[cfg(test)]
mod tests {
    use super::parse_canonical_type;

    #[test]
    fn canonical_spellings_parse() {
        assert_eq!(parse_canonical_type("0"), Some(0));
        assert_eq!(parse_canonical_type("127"), Some(127));
        assert_eq!(parse_canonical_type("-1"), Some(-1));
        assert_eq!(parse_canonical_type("-128"), Some(-128));
        assert_eq!(parse_canonical_type("42"), Some(42));
    }

    #[test]
    fn non_canonical_spellings_are_refused() {
        assert_eq!(parse_canonical_type("+1"), None);
        assert_eq!(parse_canonical_type("-01"), None);
        // `-0` would collide with `0` on the wire (both emit ext type 0).
        assert_eq!(parse_canonical_type("-0"), None);
        assert_eq!(parse_canonical_type("00"), None);
        assert_eq!(parse_canonical_type("128"), None);
        assert_eq!(parse_canonical_type("-129"), None);
        assert_eq!(parse_canonical_type(""), None);
        assert_eq!(parse_canonical_type("-"), None);
        assert_eq!(parse_canonical_type("1_0"), None);
        assert_eq!(parse_canonical_type(" 1"), None);
        assert_eq!(parse_canonical_type("1 "), None);
    }
}
