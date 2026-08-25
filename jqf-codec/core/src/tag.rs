//! Target-bound tag validation without format policy in core.
//!
//! [`NoTagsValidator`] is the default. Sibling: [`crate::encode`].

use crate::{CodecError, CodecFailureKind};
use core::any::Any;
use jqf_data::TagId;
use jqf_resource::ResourceContext;

/// Downstream target-native tag validity and collision policy.
pub trait TagValidator: Any {
    /// Validates exact stored identifiers and their set-wise native identity mapping.
    fn validate(&self, tags: &[&TagId]) -> Result<(), CodecError>;
}

impl crate::ErasedTagValidator {
    /// Constructs a checked target-bound downstream validator.
    pub fn try_new_validator<T, F>(constructor: F) -> Result<Self, CodecError>
    where
        T: TagValidator,
        F: FnOnce() -> Result<T, CodecError>,
    {
        Self::try_new_with(constructor)
    }

    /// Validates one complete exact tag set for the bound target.
    pub fn validate(&self, tags: &[&TagId], _resources: &ResourceContext<'_>) -> Result<(), CodecError> {
        self.owner.validate(tags)
    }
}

/// The no-tags target validator: authoritative tag ABSENCE. A target that cannot represent any retained tag accepts
/// exactly the empty tag set and rejects every other one. Consumers: json (both of its validators), toml, delimited,
/// ini, and jqft.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoTagsValidator;

impl TagValidator for NoTagsValidator {
    fn validate(&self, tags: &[&TagId]) -> Result<(), CodecError> {
        if tags.is_empty() {
            Ok(())
        } else {
            Err(CodecError::new(CodecFailureKind::InvalidTag))
        }
    }
}
