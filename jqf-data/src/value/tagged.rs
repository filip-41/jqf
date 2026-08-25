//! Tag names: one nonempty string with no whitespace.
//!
//! [`TagId`] is the exact text a tagged value carries. This crate does not interpret it. The decoder that produced the
//! tag owns validation, collisions, and encoding.

use core::fmt;

use crate::identity::{IdentityError, IdentityText};

/// Exact nonempty tag text, such as `!money`.
///
/// This crate does not interpret the string. The decoder owns native-tag rules.
#[derive(Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TagId(IdentityText);

impl TagId {
    /// Copy `value` into a tag. Does not charge a request ledger.
    ///
    /// A host that must account the bytes should intern the tag through the document schema instead.
    pub fn try_new_unaccounted(value: &str) -> Result<Self, TagError> {
        crate::identity::validate(value).map_err(TagError::from)?;
        IdentityText::try_new(value).map(Self).map_err(|_| TagError::Allocation)
    }

    /// The tag as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl Clone for TagId {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl TagId {
    /// Wrap a name the schema already interned and checked.
    pub(crate) fn from_accounted(value: IdentityText) -> Self {
        Self(value)
    }
}

/// Why a tag name was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TagError {
    /// Tag text was empty.
    Empty,
    /// Tag text carried an ASCII control or whitespace byte.
    InvalidCharacter,
    /// Allocation failed while retaining the exact tag text.
    Allocation,
}

impl From<IdentityError> for TagError {
    fn from(value: IdentityError) -> Self {
        match value {
            IdentityError::Empty => Self::Empty,
            IdentityError::InvalidCharacter => Self::InvalidCharacter,
        }
    }
}

impl fmt::Display for TagError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "tag must not be empty",
            Self::InvalidCharacter => "tag must not contain ASCII control or whitespace characters",
            Self::Allocation => "tag allocation failed",
        })
    }
}

impl core::error::Error for TagError {}

#[cfg(test)]
mod tests {
    use super::{TagError, TagId};

    /// A tag is an identity, so the owned constructor answers on exactly the texts the document's schema admission
    /// answers on: one text may not be a legal tag through one route and an illegal one through the other, or a value
    /// would survive being built in memory and be refused on the way into a document carrying the same tag.
    #[test]
    fn owned_tags_are_admitted_by_the_identity_grammar() {
        assert_eq!(TagId::try_new_unaccounted(""), Err(TagError::Empty));
        assert_eq!(TagId::try_new_unaccounted("!my tag"), Err(TagError::InvalidCharacter));
        assert_eq!(TagId::try_new_unaccounted("!my\ttag"), Err(TagError::InvalidCharacter));
        assert_eq!(
            TagId::try_new_unaccounted("!my\u{0}tag"),
            Err(TagError::InvalidCharacter)
        );
        assert_eq!(
            TagId::try_new_unaccounted("!money").expect("an ordinary tag").as_str(),
            "!money"
        );
        // Non-ASCII text is opaque to the grammar, not illegal.
        assert_eq!(
            TagId::try_new_unaccounted("!мон").expect("a non-ASCII tag").as_str(),
            "!мон"
        );
    }
}
